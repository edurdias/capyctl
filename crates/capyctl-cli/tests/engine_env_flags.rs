//! ADR 0028 §2.1: the engine environment by command-line flag and by
//! environment variable.
use capyctl_cli::client::{engine_env_with_channels, merge_engine_env};
use serde_json::json;

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

// T14: --engine-env is saved with the deployment under engine_config.env.
#[test]
fn engine_env_flags_merge_into_the_document() {
    let mut doc = json!({"kind": "deployment", "engine_config": {"context_length": 8192}});
    merge_engine_env(&mut doc, &[pair("SGLANG_ENABLE_X", "1")]).unwrap();
    assert_eq!(doc["engine_config"]["env"]["SGLANG_ENABLE_X"], "1");
    assert_eq!(doc["engine_config"]["context_length"], 8192);
}

// T03: a name in both the file and a flag, or twice by flag, is a conflict.
#[test]
fn file_and_flag_conflict() {
    let mut doc = json!({"engine_config": {"env": {"MBX_FUSED_DRAFT": "0"}}});
    let err = merge_engine_env(&mut doc, &[pair("MBX_FUSED_DRAFT", "1")]).unwrap_err();
    assert_eq!(err.code(), "engine_env_conflict:MBX_FUSED_DRAFT");
    let mut doc = json!({});
    let twice = [pair("A_B", "1"), pair("A_B", "2")];
    assert_eq!(
        merge_engine_env(&mut doc, &twice).unwrap_err().code(),
        "engine_env_conflict:A_B"
    );
}

// T21: an owned name by flag is refused before anything is sent.
#[test]
fn owned_name_by_flag_is_refused_locally() {
    let mut doc = json!({});
    let err = merge_engine_env(&mut doc, &[pair("SGLANG_HOST_IP", "x")]).unwrap_err();
    assert_eq!(err.code(), "engine_env_reserved:SGLANG_HOST_IP");
}

// T01: the flags parse as K=V and repeat.
#[test]
fn flags_parse() {
    assert_eq!(
        capyctl_cli::grammar::parse_env_flag("MBX_B=x=y").unwrap(),
        pair("MBX_B", "x=y")
    );
    assert!(capyctl_cli::grammar::parse_env_flag("NOEQUALS").is_err());
    assert!(capyctl_cli::grammar::parse_env_flag("=v").is_err());
}

// T14 (R9): CAPYCTL_ENGINE_ENV alone supplies entries, ';'-separated.
#[test]
fn env_var_alone_supplies_entries() {
    let merged = engine_env_with_channels(&[], Some("A_B=1;;C_D=x=y; ")).unwrap();
    assert_eq!(merged, vec![pair("A_B", "1"), pair("C_D", "x=y")]);
    let mut doc = json!({});
    merge_engine_env(&mut doc, &merged).unwrap();
    assert_eq!(doc["engine_config"]["env"]["C_D"], "x=y");
}

// R9: a name by flag and by env var is no conflict; the flag wins.
#[test]
fn flag_beats_env_var() {
    let merged = engine_env_with_channels(&[pair("A_B", "flag")], Some("A_B=env;C_D=2")).unwrap();
    let mut doc = json!({});
    merge_engine_env(&mut doc, &merged).unwrap();
    assert_eq!(doc["engine_config"]["env"]["A_B"], "flag");
    assert_eq!(doc["engine_config"]["env"]["C_D"], "2");
}

// R9, T03: a name in the YAML document and in the env var is a conflict.
#[test]
fn yaml_and_env_var_conflict() {
    let merged = engine_env_with_channels(&[], Some("MBX_FUSED_DRAFT=1")).unwrap();
    let mut doc = json!({"engine_config": {"env": {"MBX_FUSED_DRAFT": "0"}}});
    assert_eq!(
        merge_engine_env(&mut doc, &merged).unwrap_err().code(),
        "engine_env_conflict:MBX_FUSED_DRAFT"
    );
}

// R9, T21: an owned name by env var is refused before anything is sent.
#[test]
fn owned_name_by_env_var_is_refused() {
    let merged = engine_env_with_channels(&[], Some("NCCL_DEBUG=INFO")).unwrap();
    let mut doc = json!({});
    assert_eq!(
        merge_engine_env(&mut doc, &merged).unwrap_err().code(),
        "engine_env_reserved:NCCL_DEBUG"
    );
}

// R9: an env entry without a name or `=` is refused, not skipped.
#[test]
fn malformed_env_var_entry_is_refused() {
    assert!(engine_env_with_channels(&[], Some("NOEQUALS")).is_err());
    assert!(engine_env_with_channels(&[], Some("=v")).is_err());
}

fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |key| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
    }
}

// T14 (R9): engine add reads CAPYCTL_ENGINE_ADD_ENV and CAPYCTL_APPROVE_ENV;
// a flag beats the variable for one name.
#[test]
fn engine_add_channels() {
    let env = vars(&[
        ("CAPYCTL_ENGINE_ADD_ENV", "A_B=env;C_D=2"),
        ("CAPYCTL_APPROVE_ENV", "MBX_*;SGL_X"),
    ]);
    let (profile_env, approved) =
        capyctl_cli::engine::engine_add_env(&[pair("A_B", "flag")], &["MBX_*".to_owned()], &env)
            .unwrap();
    assert_eq!(profile_env["A_B"], "flag");
    assert_eq!(profile_env["C_D"], "2");
    assert_eq!(approved, vec!["MBX_*"]);
    // R9: with no --approve-env, the variable is used.
    let (_, from_variable) = capyctl_cli::engine::engine_add_env(&[], &[], &env).unwrap();
    assert_eq!(from_variable, vec!["MBX_*", "SGL_X"]);
}

// T21, T03 (R9): engine add refuses owned names, repeats, and bad approvals.
#[test]
fn engine_add_refusals() {
    let none = vars(&[]);
    let code = |r: Result<_, capyctl_config::engine_env::EnvRefusal>| match r {
        Ok(_) => "ok".to_owned(),
        Err(e) => e.code(),
    };
    assert_eq!(
        code(capyctl_cli::engine::engine_add_env(
            &[pair("NCCL_DEBUG", "1")],
            &[],
            &none
        )),
        "engine_env_reserved:NCCL_DEBUG"
    );
    let owned_var = vars(&[("CAPYCTL_ENGINE_ADD_ENV", "LD_PRELOAD=x")]);
    assert_eq!(
        code(capyctl_cli::engine::engine_add_env(&[], &[], &owned_var)),
        "engine_env_reserved:LD_PRELOAD"
    );
    assert_eq!(
        code(capyctl_cli::engine::engine_add_env(
            &[pair("A_B", "1"), pair("A_B", "2")],
            &[],
            &none
        )),
        "engine_env_conflict:A_B"
    );
    assert!(
        code(capyctl_cli::engine::engine_add_env(
            &[],
            &["*".to_owned()],
            &none
        )) != "ok"
    );
}

// T01: the flags repeat on deploy and engine add.
#[test]
fn grammar_accepts_repeated_flags() {
    use capyctl_cli::grammar::{parse, Command};
    match parse([
        "capyctl",
        "deploy",
        "model",
        "--file",
        "d.yaml",
        "--engine-env",
        "A_B=1",
        "--engine-env",
        "C_D=2",
    ]) {
        Ok(Command::Deploy { engine_env, .. }) => {
            assert_eq!(engine_env, vec![pair("A_B", "1"), pair("C_D", "2")])
        }
        other => panic!("{other:?}"),
    }
    match parse([
        "capyctl",
        "engine",
        "add",
        "--env",
        "A_B=1",
        "--env",
        "C_D=2",
        "--approve-env",
        "MBX_*",
    ]) {
        Ok(Command::EngineAdd {
            env, approved_env, ..
        }) => {
            assert_eq!(env.len(), 2);
            assert_eq!(approved_env, vec!["MBX_*"]);
        }
        other => panic!("{other:?}"),
    }
    assert!(parse(["capyctl", "deploy", "model", "--engine-env", "NOEQ"]).is_err());
}
