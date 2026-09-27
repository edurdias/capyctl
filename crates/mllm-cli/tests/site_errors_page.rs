//! Website spec, Docs ("Exit codes and errors"): every error code the CLI
//! prints and its exit status appear in docs/guide/errors.md with the number
//! the binary actually exits with.

use mllm_cli::output::{OperationError, StructuredError};

const PAGE: &str = include_str!("../../../docs/guide/errors.md");

fn all_operation_errors() -> Vec<OperationError> {
    use OperationError::*;
    let all = vec![
        InvalidConfig,
        Unauthorized,
        InsufficientResources,
        Unsupported,
        Unreconciled,
        DeviceConflict,
        CategoryLimit,
        ActivationTimeout,
        TopologyUnknown,
        NoSafeEstimate,
    ];
    // Exhaustiveness guard: a new variant fails to compile here until it is
    // added above and documented.
    for e in &all {
        match e {
            InvalidConfig
            | Unauthorized
            | InsufficientResources
            | Unsupported
            | Unreconciled
            | DeviceConflict
            | CategoryLimit
            | ActivationTimeout
            | TopologyUnknown
            | NoSafeEstimate => {}
        }
    }
    all
}

fn documented_exit(code: &str) -> Option<i32> {
    PAGE.lines()
        .filter(|l| l.starts_with('|'))
        .find(|l| l.contains(&format!("`{code}`")))
        .and_then(|l| l.split('|').nth(1))
        .and_then(|cell| cell.trim().parse().ok())
}

#[test]
fn every_operation_error_is_documented_with_its_exit_code() {
    for e in all_operation_errors() {
        assert_eq!(
            documented_exit(e.code()),
            Some(e.exit_code().0),
            "{}",
            e.code()
        );
    }
}

#[test]
fn other_structured_codes_are_documented_with_their_exit_code() {
    for code in [
        "internal",
        "not_found",
        "host_revoked",
        "host_ineligible",
        "store_from_newer_version",
        "engine_not_found",
        "engine_unsupported",
        "engine_version_failed",
        "profile_exists",
        "profile_in_use",
        "publish_rejected",
        "agent_unreachable",
        "not_interactive",
        "profile_not_published",
    ] {
        let exit = StructuredError {
            code,
            message: String::new(),
        }
        .exit_code()
        .0;
        assert_eq!(documented_exit(code), Some(exit), "{code}");
    }
}
