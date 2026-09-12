use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use mllm_cli::grammar::{self, CliError, Command, Role};
use mllm_cli::output::{self, OutputFormat, StructuredError};
use mllm_cli::roles;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let invocation = match grammar::parse_invocation(&args) {
        Ok(invocation) => invocation,
        Err(err) => return report_cli_error(&err),
    };
    let format = invocation
        .output
        .as_deref()
        .and_then(OutputFormat::from_flag)
        .unwrap_or_default();
    // The F0 exit gate wires only `start standalone`; everything else
    // still reports the structured not-yet-implemented diagnostic.
    if matches!(invocation.command, Command::Start(Role::Standalone)) {
        if invocation.config.is_some() {
            let err = StructuredError::not_yet_implemented("start standalone --config");
            output::print_error(&err, format);
            return ExitCode::from(roles::NOT_IMPLEMENTED_EXIT.0 as u8);
        }
        return run_standalone(format);
    }
    match roles::dispatch(&invocation.command) {
        Ok(never) => match never {},
        Err(err) => {
            output::print_error(&err, format);
            ExitCode::from(roles::NOT_IMPLEMENTED_EXIT.0 as u8)
        }
    }
}

/// Foreground standalone boot. F0 has no network listeners (F3 wires the
/// transports), so after a successful boot with no work in flight the
/// process exits 0 ("idle exit").
fn run_standalone(format: OutputFormat) -> ExitCode {
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
    match runtime.block_on(serve_standalone(&state_dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let err: StructuredError = err.into();
            output::print_error(&err, format);
            if err.code == "internal" {
                ExitCode::from(output::ExitCode::INTERNAL.0 as u8)
            } else {
                ExitCode::from(output::ExitCode::INVALID_CONFIG.0 as u8)
            }
        }
    }
}

/// Boot the standalone graph and serve the inference listener
/// (127.0.0.1:8443, SPEC §15.2) until the process is terminated.
async fn serve_standalone(state_dir: &std::path::Path) -> Result<(), roles::StartError> {
    let app = roles::start_standalone(state_dir).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8443")
        .await
        .map_err(roles::StartError::from)?;
    println!(
        "standalone ready (state_dir {}; inference listener 127.0.0.1:8443)",
        state_dir.display()
    );
    axum::serve(listener, app.router())
        .await
        .map_err(roles::StartError::from)
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