use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use capyctl_cli::grammar::{self, CliError, Command, Role};
use capyctl_cli::output::{self, OutputFormat, StructuredError};
use capyctl_cli::roles;
use capyctl_cli::table::{self, HostNames};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let mut invocation = match grammar::parse_invocation(&args) {
        Ok(invocation) => invocation,
        Err(err) => return report_cli_error(&err),
    };
    // ADR 0018 §2: `--config` is resolved against the working directory once,
    // here, so every command and role names the same absolute document (and
    // the engines file beside it) whatever it does later.
    invocation.config = invocation
        .config
        .as_deref()
        .map(capyctl_cli::engine::absolute);
    // Owner rule 2026-09-25: the state root, `--state-dir` > `CAPYCTL_STATE_DIR`
    // > the per-user default, resolved once for every command.
    let state_root = state_root(&invocation);
    // ADR 0021: one format rule for commands and roles.
    let role = matches!(
        invocation.command,
        Command::Start(Role::Server | Role::Host | Role::Standalone)
    );
    let format = OutputFormat::resolve(
        invocation.format.as_deref(),
        invocation.output.as_deref(),
        role,
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
    );
    // Notices from shared code (join, init, engine add) follow the same rule.
    capyctl_cli::role_text::install(format);
    let view = table::View::of(&invocation.command);
    let context = capyctl_cli::views::Context {
        names: &HostNames::default(),
        deployment_name: deployment_name(&invocation.command),
    };
    if capyctl_cli::remote_roles::supports(&invocation.command) {
        // SPEC §13.3: only a local startup flag enables full native output.
        // Clear inherited permission before creating runtime threads.
        std::env::remove_var("CAPYCTL_DEBUG_ENGINE_LOGS");
        if invocation.debug_engine_logs {
            std::env::set_var("CAPYCTL_DEBUG_ENGINE_LOGS", "1");
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Notice,
                "Full engine logs enabled in private log files; they may contain secrets.",
            );
        }
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(capyctl_cli::remote_roles::execute(&invocation, &state_root))
        {
            Ok(value) => {
                // ADR 0021: a role's shutdown summary is role output.
                if matches!(
                    invocation.command,
                    Command::Start(Role::Server | Role::Host)
                ) {
                    print!("{}", capyctl_cli::role_text::stopped(&value));
                } else {
                    emit(&invocation.command, &value, format, &context);
                }
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
        std::env::remove_var("CAPYCTL_DEBUG_ENGINE_LOGS");
        if invocation.debug_engine_logs {
            std::env::set_var("CAPYCTL_DEBUG_ENGINE_LOGS", "1");
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Notice,
                "Full engine logs enabled in private log files; they may contain secrets.",
            );
        }
        // SPEC §15.2 (R13): `--config` names the role document; without it the
        // implicit `<state_dir>/config/standalone.yaml` is loaded or generated.
        // ADR 0018 §2 (review decision 2026-09-25): `$CAPYCTL_CONFIG` names it
        // when `--config` is absent, exactly as for `capyctl engine`.
        let config =
            capyctl_cli::engine::named_role_document(invocation.config.as_deref(), &|key| {
                std::env::var(key).ok().filter(|value| !value.is_empty())
            });
        // Owner decision 2026-09-25: `--set` > `CAPYCTL_SET__…` > the document;
        // a named flag or variable of the same setting must agree.
        let overrides = match capyctl_cli::settings::role_overrides(
            capyctl_config::ConfigKind::Standalone,
            &invocation.sets,
            &capyctl_cli::settings::flag_layer(&invocation),
        ) {
            Ok(overrides) => overrides,
            Err(error) => {
                let err: StructuredError =
                    roles::StartError::Setting(capyctl_cli::settings::describe(&error)).into();
                output::print_error(&err, format);
                return ExitCode::from(err.exit_code().0 as u8);
            }
        };
        return run_standalone(
            &state_root,
            config.as_deref(),
            &invocation,
            &overrides,
            format,
        );
    }
    // ADR 0018: engine registration, on this machine, through its role's socket.
    if capyctl_cli::engine::is_engine_command(&invocation.command) {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(capyctl_cli::engine::execute(
            &invocation.command,
            invocation.config.as_deref(),
            &state_root,
        )) {
            Ok(value) => {
                emit(&invocation.command, &value, format, &context);
                // ADR 0018 §3: `engine add` with no role running says where
                // the profile was saved and what to run next.
                notice_on_stderr(&value, format);
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
        return match runtime.block_on(capyctl_cli::drain::execute(
            host.as_deref(),
            &state_root,
            invocation.config.as_deref(),
            invocation.request_id.as_deref(),
            *wait,
        )) {
            Ok(value) => {
                emit(&invocation.command, &value, format, &context);
                notice_on_stderr(&value, format);
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
        return match runtime.block_on(capyctl_cli::revoke::execute(
            host,
            &state_root,
            invocation.config.as_deref(),
            invocation.request_id.as_deref(),
        )) {
            Ok(value) => {
                emit(&invocation.command, &value, format, &context);
                notice_on_stderr(&value, format);
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
        return match runtime.block_on(capyctl_cli::prune::execute(
            host_config,
            *apply,
            referenced_file.as_deref(),
            &state_root,
            invocation.config.as_deref(),
        )) {
            Ok(value) => {
                emit(&invocation.command, &value, format, &context);
                notice_on_stderr(&value, format);
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    // SPEC §14 / §15.3: offline validation; no runtime, state, or network.
    if let Command::Validate { file, host, sets } = &invocation.command {
        // The state root a start would use, when this invocation names one.
        let named_root = invocation.state_dir.clone().or_else(|| {
            std::env::var_os("CAPYCTL_STATE_DIR")
                .filter(|dir| !dir.is_empty())
                .map(std::path::PathBuf::from)
        });
        return match capyctl_cli::validate::validate_config_at(
            file,
            host.as_deref(),
            sets,
            named_root.as_deref(),
        ) {
            Ok(value) => {
                emit(&invocation.command, &value, format, &context);
                notice_on_stderr(&value, format);
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    // Owner decision 2026-09-25: the effective configuration of a role.
    if let Command::ConfigShow { role, sets } = &invocation.command {
        return match capyctl_cli::settings::config_show(&invocation, *role, sets, &state_root) {
            Ok(value) => {
                if format == OutputFormat::Json {
                    println!("{value}");
                } else {
                    print!("{}", capyctl_cli::settings::render_table(&value));
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
    if capyctl_cli::client::supports(&invocation.command) {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(capyctl_cli::client::execute_with_start_options(
            &invocation.command,
            &state_root,
            invocation.config.as_deref(),
            invocation.request_id.as_deref(),
            invocation.initialize_timeout_ms,
            invocation.evict,
            invocation.wait,
        )) {
            Ok(value) => {
                // Text only: JSON never shows names. `--wait` results carry a
                // deployment whose instances name hosts by id.
                let wants_names = format == OutputFormat::Text
                    && (view.is_some_and(|view| view.needs_host_names())
                        || value["deployment"].is_object());
                let names = if wants_names {
                    runtime.block_on(capyctl_cli::client::host_names(
                        &state_root,
                        invocation.config.as_deref(),
                    ))
                } else {
                    Default::default()
                };
                let context = capyctl_cli::views::Context {
                    names: &names,
                    deployment_name: context.deployment_name.clone(),
                };
                emit(&invocation.command, &value, format, &context);
                warn_development_controls(&value, format);
                // ADR 0014 §7: an asynchronous deploy says what starts it.
                notice_on_stderr(&value, format);
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

/// ADR 0021: text views by default, the JSON result unchanged on request.
fn emit(
    command: &Command,
    value: &serde_json::Value,
    format: OutputFormat,
    context: &capyctl_cli::views::Context,
) {
    match format {
        OutputFormat::Json => println!("{value}"),
        OutputFormat::Text => print!("{}", capyctl_cli::views::render(command, value, context)),
    }
}

/// A result's `notice` prints once on stderr in text mode; in JSON mode it
/// stays inside the result.
fn notice_on_stderr(value: &serde_json::Value, format: OutputFormat) {
    if format == OutputFormat::Text {
        if let Some(notice) = value["notice"].as_str() {
            eprintln!("{notice}");
        }
    }
}

/// The `name` a deployment file states, for the text of `deploy`.
fn deployment_name(command: &Command) -> Option<String> {
    let Command::Deploy {
        file: Some(file), ..
    } = command
    else {
        return None;
    };
    let text = std::fs::read_to_string(file).ok()?;
    capyctl_config::parse_document(&text).ok()?["name"]
        .as_str()
        .map(str::to_owned)
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
        if let Some(notice) = capyctl_cli::exposure::status_notice(value) {
            eprintln!("{notice}");
        }
    }
}

/// Foreground standalone boot with authenticated management and inference.
fn run_standalone(
    state_dir: &std::path::Path,
    config: Option<&std::path::Path>,
    invocation: &grammar::Invocation,
    overrides: &roles::SettingOverrides,
    format: OutputFormat,
) -> ExitCode {
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
    match runtime.block_on(serve_standalone(state_dir, config, invocation, overrides)) {
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
/// `CAPYCTL_INFERENCE_ADDR` moves it for this run) until the process
/// is signalled.
///
/// SPEC §4.3 (owner decision P3): SIGTERM or SIGINT is a service restart.
/// Inference admission closes (new requests get a retryable 503), admitted
/// requests finish or are cancelled at the drain bound, the coordinator is
/// joined, and the process exits 0 with every engine still running and owned.
/// The next start adopts and re-proves them; `capyctl drain standalone` is the
/// explicit way to stop them.
async fn serve_standalone(
    state_dir: &std::path::Path,
    config: Option<&std::path::Path>,
    invocation: &grammar::Invocation,
    overrides: &roles::SettingOverrides,
) -> Result<(), roles::StartError> {
    use capyctl_cli::{exposure, shutdown};
    let listen = invocation.listen;
    let no_inference_auth = invocation.no_inference_auth;
    let models = &invocation.model_overrides;
    let engines = &invocation.engine_overrides;
    let implicit = state_dir.join("config").join("standalone.yaml");
    let bound = shutdown::standalone_drain_bound_with(config.unwrap_or(&implicit), overrides)
        .map_err(roles::StartError::Setting)?;
    // Checked before the boot, so a bad override refuses without side effects.
    roles::inference_override(listen)?;
    exposure::effective_inference_auth(exposure::InferenceAuth::ApiKey, no_inference_auth)?;
    if let Some(warning) = roles::deprecated_inference_env_warning(listen) {
        roles::role_warning(&warning);
    }
    // Owner rule 2026-09-25: a deprecated variable name is warned about once.
    for warning in
        capyctl_config::engine_settings::deprecation_warnings(&|key| std::env::var(key).ok())
    {
        roles::role_warning(&warning);
    }
    // Owner decision 2026-09-25: `--management-listen` >
    // CAPYCTL_MANAGEMENT_ADDR (or its deprecated alias) > the document, checked
    // before the boot so a bad override refuses without side effects.
    let management_override = roles::management_override(invocation.management_listen)?;
    if let Some(warning) = roles::deprecated_management_env_warning(invocation.management_listen) {
        roles::role_warning(&warning);
    }
    let mut signals = shutdown::Signals::install()?;
    // Owner decision 2026-09-25: `--models-root`, `--model-sources` and
    // `--model-sources-max` win over the environment and the document; the
    // generic overrides win over the document too.
    let app = roles::start_standalone_with_overrides(state_dir, config, models, engines, overrides)
        .await?;
    let management_address = management_override.unwrap_or(app.management_bind());
    // Design §9: `--listen` > CAPYCTL_INFERENCE_ADDR > the document.
    let inference_address = roles::effective_inference_address(app.inference_bind(), listen)?;
    // Design §9: `--no-inference-auth` > CAPYCTL_INFERENCE_AUTH > the document.
    let inference_auth =
        exposure::effective_inference_auth(app.inference_auth(), no_inference_auth)?;
    // SPEC §15.3: an accepted-but-ignored setting is reported, not silent.
    for notice in app.config_notices() {
        capyctl_domain::role_log::notice(capyctl_domain::role_log::Level::Warning, notice);
    }
    // Design §9: said out loud before the listener accepts connections.
    exposure::warn_if_exposed(inference_address, inference_auth);
    let listener = tokio::net::TcpListener::bind(inference_address)
        .await
        .map_err(|e| roles::listen_failed("inference", inference_address, e))?;
    // SPEC §16.5: management has its own loopback listener and credential.
    let management = tokio::net::TcpListener::bind(management_address)
        .await
        .map_err(|e| roles::listen_failed("management", management_address, e))?;
    roles::record_management_address(state_dir, management_address);
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
    // Design §9 ("Where the key is"): the ready line names the owner-only
    // credentials file that holds the API key, never the key.
    print!(
        "{}",
        capyctl_cli::role_text::banner(&serde_json::json!({
            "role": "standalone", "ready": true, "version": env!("CARGO_PKG_VERSION"),
            "inference": inference_address.to_string(),
            "inference_auth": if matches!(inference_auth, exposure::InferenceAuth::None) { "none" } else { "api_key" },
            "management": management_address.to_string(),
            "state_dir": state_dir, "credentials": roles::credentials_path(state_dir),
        }))
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
    print!(
        "{}",
        capyctl_cli::role_text::stopped(&serde_json::json!({
            "role": "standalone",
            "stopped": true,
            "engines": "retained",
            "drain": drain.to_json(),
            "drain_bound_secs": bound.as_secs(),
            "shutdown_ms": shutdown::elapsed_ms(started),
            "worker": format!("{worker:?}"),
        }))
    );
    Ok(())
}

/// The state root (owner decision 2026-09-25, `--set` > `CAPYCTL_SET__…` >
/// flag > environment > YAML > default): `--set state_dir=<dir>` (on `start
/// standalone` and `config show`), else `CAPYCTL_SET__STATE_DIR`, else
/// `--state-dir`, else `$CAPYCTL_STATE_DIR`, else the top-level `state_dir` of
/// the standalone document named by `--config` or `CAPYCTL_CONFIG`, else
/// `$XDG_STATE_HOME/capyctl`, else `~/.local/state/capyctl` (SPEC §16.5
/// standalone shape). It locates the implicit role documents; a standalone
/// document's `server.state_dir` and `host.state_dir` must lie under it
/// (SPEC §15.3), and a server or host document's `state_dir` is that role's
/// own state. A generic override that disagrees with `--state-dir` or
/// `CAPYCTL_STATE_DIR` refuses the start (`capyctl_cli::settings`).
fn state_root(invocation: &grammar::Invocation) -> PathBuf {
    let absolute = |dir: &str| capyctl_cli::engine::absolute(std::path::Path::new(dir));
    // A server or host document's `state_dir` is that role's own state, not
    // the root: only a standalone start (or `config show` of one) reads the
    // generic override here.
    let standalone = match &invocation.command {
        Command::Start(Role::Standalone) => true,
        Command::ConfigShow { role, .. } => match role {
            Some(role) => *role == Role::Standalone,
            None => {
                capyctl_cli::engine::named_role_document(invocation.config.as_deref(), &|key| {
                    std::env::var(key).ok().filter(|value| !value.is_empty())
                })
                .is_none_or(|named| {
                    std::fs::read_to_string(named)
                        .ok()
                        .and_then(|text| capyctl_config::parse_document(&text).ok())
                        .is_some_and(|document| document["kind"] == "standalone")
                })
            }
        },
        _ => false,
    };
    let generic = invocation
        .sets
        .iter()
        .rev()
        .find_map(|set| {
            set.split_once('=')
                .filter(|(path, _)| path.eq_ignore_ascii_case("state_dir"))
                .map(|(_, value)| value.to_owned())
        })
        .or_else(|| {
            std::env::vars().find_map(|(key, value)| {
                (key.eq_ignore_ascii_case("CAPYCTL_SET__STATE_DIR") && !value.is_empty())
                    .then_some(value)
            })
        })
        .filter(|_| standalone);
    if let Some(dir) = generic {
        return absolute(&dir);
    }
    if let Some(dir) = invocation.state_dir.as_deref() {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("CAPYCTL_STATE_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = document_state_root(invocation.config.as_deref()) {
        return dir;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) => PathBuf::from(dir).join("capyctl"),
        None => home
            .map(|home| home.join(".local").join("state").join("capyctl"))
            .unwrap_or_else(|| PathBuf::from(".capyctl-state")),
    }
}

/// The top-level `state_dir` of the standalone document named by `--config`
/// (else `CAPYCTL_CONFIG`), resolved against the document's directory.
fn document_state_root(config: Option<&std::path::Path>) -> Option<PathBuf> {
    let named = capyctl_cli::engine::named_role_document(config, &|key| {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    })?;
    let text = std::fs::read_to_string(&named).ok()?;
    let document = capyctl_config::parse_document(&text).ok()?;
    if document["kind"] != "standalone" {
        return None;
    }
    let dir = std::path::Path::new(document["state_dir"].as_str()?);
    let base = capyctl_cli::engine::absolute(&named);
    Some(match base.parent() {
        Some(parent) if dir.is_relative() => parent.join(dir),
        _ => dir.to_path_buf(),
    })
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
