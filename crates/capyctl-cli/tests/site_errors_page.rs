//! Website spec, Docs ("Exit codes and errors"): every error code the CLI
//! prints and its exit status appear in docs/guide/errors.md with the number
//! the binary actually exits with.

use capyctl_cli::output::{OperationError, StructuredError};

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

// T14 (ADR 0028 §16): every group code with an exit is on the page, in its
// documented form, with the exit the binary takes for it.
#[test]
fn group_codes_are_documented_with_their_exit_code() {
    for (documented, example) in [
        ("group_placement_required", "group_placement_required"),
        ("group_topology_invalid", "group_topology_invalid"),
        ("group_profile_mismatch", "group_profile_mismatch"),
        ("group_checkpoint_mismatch", "group_checkpoint_mismatch"),
        ("group_model_path_mismatch", "group_model_path_mismatch"),
        ("peer_address_missing", "peer_address_missing"),
        ("peer_address_not_local", "peer_address_not_local"),
        (
            "engine_env_reserved:<name>",
            "engine_env_reserved:NCCL_DEBUG",
        ),
        (
            "engine_env_not_approved:<name>",
            "engine_env_not_approved:X",
        ),
        ("engine_env_conflict:<name>", "engine_env_conflict:X"),
        ("rendezvous_ports_exhausted", "rendezvous_ports_exhausted"),
        (
            "rendezvous_port_in_use:<port>",
            "rendezvous_port_in_use:25000",
        ),
        ("service_port_in_use:<port>", "service_port_in_use:8100"),
        ("host_tuning_missing:<item>", "host_tuning_missing:memlock"),
        ("group_shape_unsupported", "group_shape_unsupported"),
        (
            "group_shape_unsupported:<engine>",
            "group_shape_unsupported:tensorfold",
        ),
        ("group_instances_unsupported", "group_instances_unsupported"),
        ("group_drift:<field>", "group_drift:node_rank"),
        (
            "host_capability_missing:engine_groups",
            "host_capability_missing:engine_groups",
        ),
    ] {
        let exit = StructuredError {
            code: "invalid_config",
            message: format!("{example}: detail"),
        }
        .exit_code()
        .0;
        assert_eq!(documented_exit(documented), Some(exit), "{documented}");
    }
}
