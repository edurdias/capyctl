use std::ffi::OsString;
use std::process::ExitCode;

use mllm_cli::grammar::{self, CliError};
use mllm_cli::output::{self, OutputFormat};
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
    match roles::dispatch(&invocation.command) {
        Ok(never) => match never {},
        Err(err) => {
            output::print_error(&err, format);
            ExitCode::from(roles::NOT_IMPLEMENTED_EXIT.0 as u8)
        }
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