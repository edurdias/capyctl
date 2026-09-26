use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use mllm_cli::grammar::{self, CliError, Command, Role};
use mllm_cli::output::{self, OutputFormat, StructuredError};
use mllm_cli::roles;
use mllm_cli::table::{self, HostNames, View};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let mut invocation = match grammar::parse_invocation(&args) {
        Ok(invocation) => invocation,
        Err(err) => return report_cli_error(&err),
    };
    // ADR 0018 §2: `--config` is resolved against the working directory once,
    // here, so every command and role names the same absolute document (and
    // the engines file beside it) whatever it does later.
    invocation.config = invocation.config.as_deref().map(mllm_cli::engine::absolute);
    // Owner decision 2026-09-25: `--format json` is machine mode, exactly as
    // `--output json` was (and still is): JSON results and JSON errors.
    let format = match invocation.format.as_deref() {
        Some("json") => OutputFormat::Json,
        _ => invocation
            .output
            .as_deref()
            .and_then(OutputFormat::from_flag)
            .unwrap_or_default(),
    };
    let json_records = match invocation.format.as_deref() {
        Some(explicit) => explicit == "json",
        None => invocation.output.as_deref() == Some("json"),
    };
    let view = View::of(&invocation.command).filter(|_| !json_records);
    if mllm_cli::remote_roles::supports(&invocation.command) {
        // SPEC §13.3: only a local startup flag enables full native output.
        // Clear inherited permission before creating runtime threads.
        std::env::remove_var("MLLM_DEBUG_ENGINE_LOGS");
        if invocation.debug_engine_logs {
            std::env::set_var("MLLM_DEBUG_ENGINE_LOGS", "1");
            eprintln!("Full engine logs enabled in private log files; they may contain secrets.");
        }
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(mllm_cli::remote_roles::execute(
            &invocation,
            &default_state_dir(),
        )) {
            Ok(value) => {
                emit(&value, view, &Default::default());
                warn_development_controls(&value, format);
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    // Standalone owns both listeners; client commands use its management API.
    if matches!(invocation.command, Command::Start(Role::Standalone)) {
        // Set before constructing any runtime threads. A shell variable alone
        // cannot enable full native logs; the operator must pass the flag.
        std::env::remove_var("MLLM_DEBUG_ENGINE_LOGS");
        if invocation.debug_engine_logs {
            std::env::set_var("MLLM_DEBUG_ENGINE_LOGS", "1");
            eprintln!("Full engine logs enabled in private log files; they may contain secrets.");
        }
        // SPEC §15.2 (R13): `--config` names the role document; without it the
        // implicit `<state_dir>/config/standalone.yaml` is loaded or generated.
        // ADR 0018 §2 (review decision 2026-09-25): `$MLLM_CONFIG` names it
        // when `--config` is absent, exactly as for `mllm engine`.
        let config = mllm_cli::engine::named_role_document(invocation.config.as_deref(), &|key| {
            std::env::var(key).ok().filter(|value| !value.is_empty())
        });
        return run_standalone(
            config.as_deref(),
            invocation.listen,
            invocation.no_inference_auth,
            format,
        );
    }
    // ADR 0018: engine registration, on this machine, through its role's socket.
    if mllm_cli::engine::is_engine_command(&invocation.command) {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(mllm_cli::engine::execute(
            &invocation.command,
            invocation.config.as_deref(),
            &default_state_dir(),
        )) {
            Ok(value) => {
                emit(&value, view, &Default::default());
                // ADR 0018 §3: `engine add` with no role running says where
                // the profile was saved and what to run next.
                if format == OutputFormat::Text {
                    if let Some(notice) = value["notice"].as_str() {
                        eprintln!("{notice}");
                    }
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    // SPEC §4.3: explicit drain, through the server's or the standalone role's
    // management API. Stopping a role itself is a signal (SPEC §14 has no verb).
    if let Command::Drain { host, wait } = &invocation.command {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(mllm_cli::drain::execute(
            host.as_deref(),
            &default_state_dir(),
            invocation.config.as_deref(),
            invocation.request_id.as_deref(),
            *wait,
        )) {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    // SPEC §§4.1, 13.3: revoke an enrolled host through the server's
    // management API; its session closes and it takes no new work.
    if let Command::Revoke { host } = &invocation.command {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(mllm_cli::revoke::execute(
            host,
            &default_state_dir(),
            invocation.config.as_deref(),
            invocation.request_id.as_deref(),
        )) {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    // SPEC §6.3, ADR 0008: explicit, host-side reclaim of unreferenced
    // materialized model sources.
    if let Command::PruneSources {
        host_config,
        apply,
        referenced_file,
    } = &invocation.command
    {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(mllm_cli::prune::execute(
            host_config,
            *apply,
            referenced_file.as_deref(),
            &default_state_dir(),
            invocation.config.as_deref(),
        )) {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    // SPEC §14 / §15.3: offline validation; no runtime, state, or network.
    if let Command::Validate { file, host } = &invocation.command {
        return match mllm_cli::validate::validate_config(file, host.as_deref()) {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    if mllm_cli::client::supports(&invocation.command) {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(mllm_cli::client::execute_with_start_options(
            &invocation.command,
            &default_state_dir(),
            invocation.config.as_deref(),
            invocation.request_id.as_deref(),
            invocation.initialize_timeout_ms,
            invocation.evict,
            invocation.wait,
        )) {
            Ok(value) => {
                let names = match view {
                    Some(view) if view.needs_host_names() => {
                        runtime.block_on(mllm_cli::client::host_names(
                            &default_state_dir(),
                            invocation.config.as_deref(),
                        ))
                    }
                    _ => Default::default(),
                };
                emit(&value, view, &names);
                warn_development_controls(&value, format);
                // ADR 0014 §7: an asynchronous deploy says what starts it.
                if format == OutputFormat::Text {
                    if let Some(notice) = value["notice"].as_str() {
                        eprintln!("{notice}");
                    }
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    match roles::dispatch(&invocation.command) {
        Ok(never) => match never {},
        Err(err) => {
            output::print_error(&err, format);
            ExitCode::from(roles::NOT_IMPLEMENTED_EXIT.0 as u8)
        }
    }
}

/// Owner decision 2026-09-25: a record view prints as a table unless JSON was
/// asked for; everything else prints its JSON result, as it always has.
fn emit(value: &serde_json::Value, view: Option<View>, names: &HostNames) {
    match view {
        Some(view) => print!("{}", table::render(view, value, names)),
        None => println!("{value}"),
    }
}

/// SPEC §9.1 / T21 / P4: in text mode, status and inspect views warn on stderr
/// about every deployment or host installation exposing vLLM development
/// controls, and (design §9) about an inference endpoint served on a
/// non-loopback address without the API key. The JSON result on stdout is
/// unchanged.
fn warn_development_controls(value: &serde_json::Value, format: OutputFormat) {
    if format == OutputFormat::Text {
        for notice in output::development_controls_notices(value) {
            eprintln!("{notice}");
        }
        // Design §9: status repeats the unauthenticated-exposure warning.
        if let Some(notice) = mllm_cli::exposure::status_notice(value) {
            eprintln!("{notice}");
        }
    }
}

/// Foreground standalone boot with authenticated management and inference.
fn run_standalone(
    config: Option<&std::path::Path>,
    listen: Option<std::net::SocketAddr>,
    no_inference_auth: bool,
    format: OutputFormat,
) -> ExitCode {
    let state_dir = default_state_dir();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let err = StructuredError {
                code: "internal",
                message: format!("failed to start async runtime: {e}"),
            };
            output::print_error(&err, format);
            return ExitCode::from(output::ExitCode::INTERNAL.0 as u8);
        }
    };
    match runtime.block_on(serve_standalone(
        &state_dir,
        config,
        listen,
        no_inference_auth,
    )) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let err: StructuredError = err.into();
            output::print_error(&err, format);
            ExitCode::from(err.exit_code().0 as u8)
        }
    }
}

/// Boot the standalone graph and serve the inference listener (design §9:
/// the document's bind, `0.0.0.0:8443` by default, unless `--listen` or
/// `MLLM_INFERENCE_ADDR` moves it for this run) until the process
/// is signalled.
///
/// SPEC §4.3 (owner decision P3): SIGTERM or SIGINT is a service restart.
/// Inference admission closes (new requests get a retryable 503), admitted
/// requests finish or are cancelled at the drain bound, the coordinator is
/// joined, and the process exits 0 with every engine still running and owned.
/// The next start adopts and re-proves them; `mllm drain standalone` is the
/// explicit way to stop them.
async fn serve_standalone(
    state_dir: &std::path::Path,
    config: Option<&std::path::Path>,
    listen: Option<std::net::SocketAddr>,
    no_inference_auth: bool,
) -> Result<(), roles::StartError> {
    use mllm_cli::{exposure, shutdown};
    let bound = match config {
        Some(document) => shutdown::standalone_drain_bound_in(document),
        None => shutdown::standalone_drain_bound(state_dir),
    }
    .map_err(roles::StartError::Setting)?;
    // Checked before the boot, so a bad override refuses without side effects.
    roles::inference_override(listen)?;
    exposure::effective_inference_auth(exposure::InferenceAuth::ApiKey, no_inference_auth)?;
    if let Some(warning) = roles::deprecated_inference_env_warning(listen) {
        eprintln!("{warning}");
    }
    let management_address = roles::standalone_management_address()?;
    let mut signals = shutdown::Signals::install()?;
    let app = roles::start_standalone_from(state_dir, config).await?;
    // Design §9: `--listen` > MLLM_INFERENCE_ADDR > the document.
    let inference_address = roles::effective_inference_address(app.inference_bind(), listen)?;
    // Design §9: `--no-inference-auth` > MLLM_INFERENCE_AUTH > the document.
    let inference_auth =
        exposure::effective_inference_auth(app.inference_auth(), no_inference_auth)?;
    // SPEC §15.3: an accepted-but-ignored setting is reported, not silent.
    for notice in app.config_notices() {
        eprintln!("warning: {notice}");
    }
    // Design §9: said out loud before the listener accepts connections.
    exposure::warn_if_exposed(inference_address, inference_auth);
    let listener = tokio::net::TcpListener::bind(inference_address)
        .await
        .map_err(roles::StartError::from)?;
    // SPEC §16.5: management has its own loopback listener and credential.
    let management = tokio::net::TcpListener::bind(management_address)
        .await
        .map_err(roles::StartError::from)?;
    let admission = shutdown::Admission::new();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mut inference = tokio::spawn(shutdown::serve(
        listener,
        admission.gate(app.inference_router(inference_address, inference_auth)),
        stopped.clone(),
    ));
    let mut control = tokio::spawn(shutdown::serve(
        management,
        app.management_router(),
        stopped,
    ));
    println!(
        "standalone ready (state_dir {}; inference listener {inference_address})",
        state_dir.display()
    );
    let failed = tokio::select! {
        _ = signals.recv() => None,
        result = &mut inference => Some(result),
        result = &mut control => Some(result),
    };
    if let Some(result) = failed {
        let _ = app.shutdown().await;
        return Err(match result {
            Ok(Err(error)) => roles::StartError::from(error),
            _ => roles::StartError::Deploy("a listener stopped unexpectedly".into()),
        });
    }
    let started = std::time::Instant::now();
    let drain = admission.drain_unless(bound, signals.forced()).await;
    stop.send_replace(true);
    let _ = shutdown::join_listeners(async {
        let _ = (&mut inference).await;
        let _ = (&mut control).await;
    })
    .await;
    inference.abort();
    control.abort();
    let worker = app.shutdown().await;
    println!(
        "{}",
        serde_json::json!({
            "role": "standalone",
            "stopped": true,
            "engines": "retained",
            "drain": drain.to_json(),
            "drain_bound_secs": bound.as_secs(),
            "shutdown_ms": shutdown::elapsed_ms(started),
            "worker": format!("{worker:?}"),
        })
    );
    Ok(())
}

/// F0 default state root: `$MLLM_STATE_DIR`, else `$XDG_STATE_HOME/mllm`,
/// else `~/.local/state/mllm` (SPEC §16.5 standalone shape).
fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MLLM_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) => PathBuf::from(dir).join("mllm"),
        None => home
            .map(|home| home.join(".local").join("state").join("mllm"))
            .unwrap_or_else(|| PathBuf::from(".mllm-state")),
    }
}

fn report_cli_error(err: &CliError) -> ExitCode {
    match err {
        CliError::Clap(clap_err) => {
            let _ = clap_err.print();
            if clap_err.use_stderr() {
                ExitCode::from(output::exit_code_for_cli_error(err).0 as u8)
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}
