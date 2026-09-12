use mllm_cli::grammar::parse;
use mllm_cli::output::{exit_code_for_cli_error, ExitCode, OperationError, OutputFormat, StructuredError};

#[test]
fn exit_code_table_matches_design_section_7() {
    assert_eq!(OperationError::InvalidConfig.exit_code(), ExitCode(2));
    assert_eq!(OperationError::Unauthorized.exit_code(), ExitCode(3));
    assert_eq!(OperationError::InsufficientResources.exit_code(), ExitCode(4));
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
    let err = parse(["mllm", "server", "run"]).unwrap_err();
    assert_eq!(exit_code_for_cli_error(&err), ExitCode(2));
    let err = parse(["mllm", "stop", "model", "dep_x"]).unwrap_err();
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
    let internal = StructuredError { code: "internal", message: "store: sqlite".into() };
    assert_eq!(internal.exit_code(), ExitCode(13));
    let config = StructuredError { code: "invalid_config", message: "bad yaml".into() };
    assert_eq!(config.exit_code(), ExitCode(2));
    assert_ne!(internal.exit_code(), config.exit_code());
}
