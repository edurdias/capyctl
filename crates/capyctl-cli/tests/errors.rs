use capyctl_cli::grammar::parse;
use capyctl_cli::output::{
    exit_code_for_cli_error, ExitCode, OperationError, OutputFormat, StructuredError,
};

#[test]
fn exit_code_table_matches_design_section_7() {
    assert_eq!(OperationError::InvalidConfig.exit_code(), ExitCode(2));
    assert_eq!(OperationError::Unauthorized.exit_code(), ExitCode(3));
    assert_eq!(
        OperationError::InsufficientResources.exit_code(),
        ExitCode(4)
    );
    assert_eq!(OperationError::Unsupported.exit_code(), ExitCode(5));
    assert_eq!(OperationError::Unreconciled.exit_code(), ExitCode(6));
    assert_eq!(OperationError::DeviceConflict.exit_code(), ExitCode(7));
    assert_eq!(OperationError::CategoryLimit.exit_code(), ExitCode(8));
    assert_eq!(OperationError::ActivationTimeout.exit_code(), ExitCode(10));
    assert_eq!(OperationError::TopologyUnknown.exit_code(), ExitCode(11));
    assert_eq!(OperationError::NoSafeEstimate.exit_code(), ExitCode(12));
    assert_eq!(ExitCode::SUCCESS, ExitCode(0));
}

#[test]
fn parse_errors_map_to_invalid_config() {
    let err = parse(["capyctl", "server", "run"]).unwrap_err();
    assert_eq!(exit_code_for_cli_error(&err), ExitCode(2));
    let err = parse(["capyctl", "stop", "model", "dep_x"]).unwrap_err();
    assert_eq!(exit_code_for_cli_error(&err), ExitCode(2));
}

#[test]
fn structured_error_json_shape() {
    let e = StructuredError {
        code: "not_implemented",
        message: "stop deployment \"x\"\nsecond\tline".to_string(),
    };
    let json = e.to_json();
    assert!(json.starts_with('{') && json.ends_with('}'));
    assert!(json.contains("\"code\":\"not_implemented\""));
    assert!(json.contains("\"message\":\"stop deployment \\\"x\\\"\\nsecond\\tline\""));
}

#[test]
fn not_yet_implemented_error() {
    let e = StructuredError::not_yet_implemented("start server");
    assert_eq!(e.code, "not_implemented");
    assert_eq!(e.exit_code(), ExitCode(5));
    assert!(e.message.contains("start server"));
}

#[test]
fn output_format_flag_parsing() {
    assert_eq!(OutputFormat::from_flag("json"), Some(OutputFormat::Json));
    assert_eq!(OutputFormat::from_flag("text"), Some(OutputFormat::Text));
    assert_eq!(OutputFormat::from_flag("server.yaml"), None);
    assert_eq!(OutputFormat::default(), OutputFormat::Text);
}
#[test]
fn internal_failures_exit_13_not_invalid_config() {
    // F1 design §4: store/I-O/runtime-boot failures get a distinct internal
    // code so scripts never mistake a broken state dir for bad config.
    assert_eq!(ExitCode::INTERNAL.0, 13);
    let internal = StructuredError {
        code: "internal",
        message: "store: sqlite".into(),
    };
    assert_eq!(internal.exit_code(), ExitCode(13));
    let config = StructuredError {
        code: "invalid_config",
        message: "bad yaml".into(),
    };
    assert_eq!(config.exit_code(), ExitCode(2));
    assert_ne!(internal.exit_code(), config.exit_code());
}

// T33 (SPEC §13.2): a store written by a newer capyctl is reported with its own
// code and a recovery hint, and exits as unsupported (5), which the packaged
// service units do not restart: restarting the same binary never heals it.
#[test]
fn a_store_from_a_newer_version_is_a_non_restartable_refusal() {
    let newer = capyctl_store::StoreError::FromNewerVersion {
        found: 99,
        supported: 31,
    };
    for error in [
        capyctl_cli::roles::StartError::Store(newer),
        capyctl_cli::roles::StartError::Ownership(capyctl_controller::OwnedStateError::Store(
            capyctl_store::StoreError::FromNewerVersion {
                found: 99,
                supported: 31,
            },
        )),
    ] {
        let e = StructuredError::from(error);
        assert_eq!(e.code, capyctl_cli::output::STORE_FROM_NEWER_VERSION);
        assert_eq!(e.exit_code(), ExitCode(5));
        assert!(
            e.message.contains("99") && e.message.contains("31"),
            "{}",
            e.message
        );
        assert!(e.message.contains("backup"), "{}", e.message);
    }
}

// T06 (SPEC §4.1, ADR 0016, owner decision 2026-09-24): a host the controller
// revoked exits with its own code, distinct from every other one, and says
// how to recover with the real CLI verbs. The packaged host units list this
// code in RestartPreventExitStatus (scripts/verify-packaging.sh checks it).
#[test]
fn a_revoked_host_exits_with_its_own_code_and_the_recovery_commands() {
    assert_eq!(ExitCode::HOST_REVOKED, ExitCode(14));
    for other in [
        ExitCode::SUCCESS,
        ExitCode::INVALID_CONFIG,
        ExitCode::UNAUTHORIZED,
        ExitCode::INSUFFICIENT_RESOURCES,
        ExitCode::UNSUPPORTED,
        ExitCode::UNRECONCILED,
        ExitCode::DEVICE_CONFLICT,
        ExitCode::CATEGORY_LIMIT,
        ExitCode::ACTIVATION_TIMEOUT,
        ExitCode::TOPOLOGY_UNKNOWN,
        ExitCode::NO_SAFE_ESTIMATE,
        ExitCode::INTERNAL,
        ExitCode::HOST_INELIGIBLE,
    ] {
        assert_ne!(other, ExitCode::HOST_REVOKED);
    }
    let e = capyctl_cli::remote_roles::host_revoked("01HOSTID");
    assert_eq!(e.code, capyctl_cli::output::HOST_REVOKED);
    assert_eq!(e.exit_code(), ExitCode(14));
    assert!(!e.message.contains('\n'), "one line: {}", e.message);
    assert!(e.message.contains("engines keep running"), "{}", e.message);
    assert!(
        e.message
            .contains("capyctl invite host 01HOSTID --recover --output FILE"),
        "{}",
        e.message
    );
    assert!(
        e.message
            .contains("capyctl join host --join-file FILE --recover"),
        "{}",
        e.message
    );
    // The commands named in the message parse with the real grammar.
    parse([
        "capyctl",
        "invite",
        "host",
        "01HOSTID",
        "--recover",
        "--output",
        "FILE",
    ])
    .unwrap();
    parse([
        "capyctl",
        "join",
        "host",
        "--join-file",
        "FILE",
        "--recover",
    ])
    .unwrap();
}

// T23 (owner decision 2026-09-25): a start refused because no allowed host is
// eligible exits with its own code, 15, distinct from every other one and in
// particular from the revoked host's 14 (9 stays a reserved gap).
#[test]
fn a_start_with_no_eligible_host_exits_15() {
    assert_eq!(ExitCode::HOST_INELIGIBLE, ExitCode(15));
    let e = StructuredError {
        code: "host_ineligible",
        message: "host_ineligible: no allowed host is eligible for placement".into(),
    };
    assert_eq!(e.exit_code(), ExitCode(15));
    for other in [
        ExitCode::SUCCESS,
        ExitCode::INVALID_CONFIG,
        ExitCode::UNAUTHORIZED,
        ExitCode::INSUFFICIENT_RESOURCES,
        ExitCode::UNSUPPORTED,
        ExitCode::UNRECONCILED,
        ExitCode::DEVICE_CONFLICT,
        ExitCode::CATEGORY_LIMIT,
        ExitCode::ACTIVATION_TIMEOUT,
        ExitCode::TOPOLOGY_UNKNOWN,
        ExitCode::NO_SAFE_ESTIMATE,
        ExitCode::INTERNAL,
        ExitCode::HOST_REVOKED,
        ExitCode::ENGINE_NOT_FOUND,
        ExitCode::ENGINE_UNSUPPORTED,
        ExitCode::ENGINE_VERSION_FAILED,
        ExitCode::PROFILE_EXISTS,
        ExitCode::PROFILE_IN_USE,
        ExitCode::PUBLISH_REJECTED,
        ExitCode::AGENT_UNREACHABLE,
        ExitCode::NOT_INTERACTIVE,
        ExitCode::PROFILE_NOT_PUBLISHED,
    ] {
        assert_ne!(other, ExitCode::HOST_INELIGIBLE);
    }
}

// T01 (ADR 0018 §6): the engine codes exit 16 to 23; 9 stays unused.
#[test]
fn engine_codes_have_their_exit_codes() {
    for (code, exit) in [
        ("engine_not_found", 16),
        ("engine_unsupported", 17),
        ("engine_version_failed", 18),
        ("profile_exists", 19),
        ("profile_in_use", 20),
        ("publish_rejected", 21),
        ("agent_unreachable", 22),
        ("not_interactive", 23),
    ] {
        let error = StructuredError {
            code,
            message: String::new(),
        };
        assert_eq!(error.exit_code(), ExitCode(exit), "{code}");
        assert_ne!(error.exit_code(), ExitCode(9));
    }
}

// T01 (ADR 0018 §7): a deploy refused for an unpublished profile exits 24.
#[test]
fn profile_not_published_exits_24() {
    let error = StructuredError {
        code: "profile_not_published",
        message: String::new(),
    };
    assert_eq!(error.exit_code(), ExitCode(24));
    assert_eq!(ExitCode::PROFILE_NOT_PUBLISHED, ExitCode(24));
}

// T26 T29 (design §11): the discrete GPU closed codes travel inside a
// refusal's message (a management `invalid_config` or `command_rejected`, a
// failed operation, a boot error). The CLI exits with the code's class: 4 for
// memory, 5 for unsupported, 2 for configuration. No new exit number.
#[test]
fn discrete_gpu_codes_exit_with_their_spec_class() {
    let cases = [
        ("insufficient_device_memory", 4),
        ("device_unobserved", 4),
        ("multi_gpu_unsupported", 5),
        ("unsupported_gpu_topology", 5),
        ("host_backed_unavailable", 5),
        ("device_policy_mismatch", 2),
        ("missing_system_allocation", 2),
    ];
    for (code, exit) in cases {
        // The code itself.
        let direct = StructuredError {
            code,
            message: "refused".into(),
        };
        assert_eq!(direct.exit_code(), ExitCode(exit), "{code}");
        // As a detail prefix under each generic class that carries it.
        for carrier in [
            "invalid_config",
            "command_rejected",
            "operation_failed",
            "internal",
            "management_unavailable",
            "insufficient_resources",
        ] {
            let wrapped = StructuredError {
                code: carrier,
                message: format!(
                    "invalid_config: Invalid deployment configuration: unsupported combination \
                     at `resources`: {code}: the detail"
                ),
            };
            assert_eq!(wrapped.exit_code(), ExitCode(exit), "{carrier} {code}");
            assert_eq!(wrapped.closed_code(), code, "{carrier} {code}");
        }
    }
    // A code named without its detail colon, or inside a longer word, is not
    // the refusal's code.
    let prose = StructuredError {
        code: "invalid_config",
        message: "see device_unobserved docs; xdevice_unobserved: no".into(),
    };
    assert_eq!(prose.exit_code(), ExitCode(2));
    // A specific code is never overridden by its message.
    let specific = StructuredError {
        code: "unauthorized",
        message: "device_unobserved: x".into(),
    };
    assert_eq!(specific.exit_code(), ExitCode(3));
}
