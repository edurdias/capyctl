//! ADR 0024 (owner decision 2026-10-03): tool-call and reasoning parsers chosen
//! by model family for vLLM and SGLang. CPU-only tests; none of this is
//! qualification of a native engine recipe.

use std::path::Path;

use capyctl_config::effective::resolve_effective;
use capyctl_config::parsers::{
    parsers_for_launch, parsers_on_remote_host, vllm_auto_tool_choice, ParserSource,
};
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"]["args"] = json!([]);
    (deployment, host)
}

fn on(engine: &str) -> (Value, Value) {
    let (deployment, mut host) = fixture();
    if engine == "sglang" {
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = "sglang".into();
        profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
    }
    (deployment, host)
}

/// The FrogNano 4B checkpoint's shape: Qwen3.5 architecture, a template with
/// the XML tool-call markup and a thinking block.
const QWEN3_5_TEMPLATE: &str = "{% if tools %}<tools>{{ tools }}</tools> <tool_call>\
    <function=name><parameter=x>1</parameter></function></tool_call>{% endif %}\
    <think>\n";
/// Qwen3 (2025) hybrid-thinking checkpoints: JSON tool calls in `<tool_call>`.
const QWEN3_TEMPLATE: &str = "<tool_call>\n{\"name\": ...}\n</tool_call><think>";

fn checkpoint(config: Value, template: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    if let Some(template) = template {
        std::fs::write(
            dir.path().join("tokenizer_config.json"),
            json!({"chat_template": template}).to_string(),
        )
        .unwrap();
    }
    dir
}

fn qwen3_5() -> tempfile::TempDir {
    checkpoint(
        json!({"architectures": ["Qwen3_5ForConditionalGeneration"], "model_type": "qwen3_5"}),
        Some(QWEN3_5_TEMPLATE),
    )
}

fn resolve(
    engine: &str,
    edit: impl FnOnce(&mut Value),
) -> capyctl_config::effective::EffectiveDeployment {
    let (mut deployment, host) = on(engine);
    edit(&mut deployment);
    resolve_effective(&deployment, &host).expect("resolves")
}

fn launch(
    effective: &capyctl_config::effective::EffectiveDeployment,
    root: &Path,
) -> capyctl_config::parsers::Parsers {
    parsers_for_launch(
        effective.profile.engine,
        &effective.engine_config,
        &effective.profile.args,
        Some(root),
    )
    .expect("vLLM and SGLang choose parsers")
}

// T14: the Qwen3.5 family (Qwen3.5, Qwen3.6, Qwen3.8) gets the XML tool parser
// and the qwen3 reasoning parser on both engines, with nothing declared.
#[test]
fn qwen3_5_family_gets_its_parsers_on_both_engines() {
    let dir = qwen3_5();
    for engine in ["vllm", "sglang"] {
        let effective = resolve(engine, |_| {});
        let parsers = launch(&effective, dir.path());
        assert_eq!(parsers.family.as_deref(), Some("qwen3_5"), "{engine}");
        assert_eq!(
            parsers.tool_call.name.as_deref(),
            Some("qwen3_coder"),
            "{engine}"
        );
        assert_eq!(parsers.tool_call.source, ParserSource::ModelFamily);
        assert_eq!(parsers.reasoning.name.as_deref(), Some("qwen3"), "{engine}");
        assert_eq!(parsers.reasoning.source, ParserSource::ModelFamily);
    }
    let effective = resolve("vllm", |_| {});
    let parsers = launch(&effective, dir.path());
    assert!(vllm_auto_tool_choice(
        &parsers,
        &effective.profile.args,
        effective.engine_config.extra_args()
    ));
}

// T14: the Qwen3 family uses JSON tool calls: vLLM `hermes`, SGLang `qwen25`.
// The MoE variant and the architecture name alone are recognised too.
#[test]
fn qwen3_family_gets_the_json_tool_parser() {
    for config in [
        json!({"architectures": ["Qwen3ForCausalLM"], "model_type": "qwen3"}),
        json!({"architectures": ["Qwen3MoeForCausalLM"], "model_type": "qwen3_moe"}),
        json!({"architectures": ["Qwen3ForCausalLM"]}),
    ] {
        let dir = checkpoint(config.clone(), Some(QWEN3_TEMPLATE));
        for (engine, tool) in [("vllm", "hermes"), ("sglang", "qwen25")] {
            let parsers = launch(&resolve(engine, |_| {}), dir.path());
            assert_eq!(parsers.family.as_deref(), Some("qwen3"), "{config}");
            assert_eq!(
                parsers.tool_call.name.as_deref(),
                Some(tool),
                "{engine} {config}"
            );
            assert_eq!(parsers.reasoning.name.as_deref(), Some("qwen3"), "{engine}");
        }
    }
}

// T14: the template decides what the model emits. An instruct checkpoint with
// no thinking block gets no reasoning parser; a Qwen3 checkpoint whose
// template uses the XML markup (Qwen3-Coder) gets the XML tool parser; one
// with no tool markup gets no tool parser; a template in
// `chat_template.jinja` is read too.
#[test]
fn the_chat_template_decides_which_parsers_apply() {
    let instruct = checkpoint(
        json!({"model_type": "qwen3"}),
        Some("<tool_call>{}</tool_call>"),
    );
    let parsers = launch(&resolve("vllm", |_| {}), instruct.path());
    assert_eq!(parsers.tool_call.name.as_deref(), Some("hermes"));
    assert_eq!(parsers.reasoning.name, None);
    assert_eq!(parsers.reasoning.source, ParserSource::Unsupported);
    assert!(parsers
        .reasoning
        .reason
        .as_deref()
        .unwrap()
        .contains("thinking"));

    let coder = checkpoint(
        json!({"model_type": "qwen3_moe"}),
        Some("<tool_call><function=f>"),
    );
    let parsers = launch(&resolve("sglang", |_| {}), coder.path());
    assert_eq!(parsers.tool_call.name.as_deref(), Some("qwen3_coder"));

    let plain = checkpoint(json!({"model_type": "qwen3_5"}), Some("<think>"));
    let parsers = launch(&resolve("vllm", |_| {}), plain.path());
    assert_eq!(parsers.tool_call.name, None);
    assert_eq!(parsers.tool_call.source, ParserSource::Unsupported);
    assert_eq!(parsers.reasoning.name.as_deref(), Some("qwen3"));

    let jinja = checkpoint(json!({"model_type": "qwen3_5"}), None);
    std::fs::write(jinja.path().join("chat_template.jinja"), QWEN3_5_TEMPLATE).unwrap();
    let parsers = launch(&resolve("vllm", |_| {}), jinja.path());
    assert_eq!(parsers.tool_call.name.as_deref(), Some("qwen3_coder"));

    let none = checkpoint(json!({"model_type": "qwen3_5"}), None);
    let parsers = launch(&resolve("vllm", |_| {}), none.path());
    assert_eq!(
        (parsers.tool_call.name, parsers.reasoning.name),
        (None, None)
    );
    assert!(parsers
        .tool_call
        .reason
        .as_deref()
        .unwrap()
        .contains("chat template"));
}

// T14: an unknown family, or a checkpoint that cannot be read, keeps today's
// behaviour: no parser, with the reason.
#[test]
fn an_unknown_family_gets_no_parsers() {
    let llama = checkpoint(
        json!({"architectures": ["LlamaForCausalLM"], "model_type": "llama"}),
        Some(QWEN3_TEMPLATE),
    );
    let parsers = launch(&resolve("vllm", |_| {}), llama.path());
    assert_eq!(parsers.family, None);
    assert_eq!(parsers.tool_call.name, None);
    assert_eq!(parsers.tool_call.source, ParserSource::UnknownFamily);
    assert!(parsers
        .tool_call
        .reason
        .as_deref()
        .unwrap()
        .contains("llama"));
    assert_eq!(parsers.reasoning.name, None);

    let empty = tempfile::tempdir().unwrap();
    let parsers = launch(&resolve("sglang", |_| {}), empty.path());
    assert_eq!(
        (parsers.tool_call.name, parsers.reasoning.name),
        (None, None)
    );
    assert_eq!(parsers.tool_call.source, ParserSource::UnknownFamily);
}

// T14: `none` turns a parser off, and a named parser is used as written.
#[test]
fn the_deployment_can_name_or_turn_off_each_parser() {
    let dir = qwen3_5();
    for engine in ["vllm", "sglang"] {
        let effective = resolve(engine, |d| {
            d["engine_config"][engine] = json!({"tool_call_parser": "none",
                "reasoning_parser": "deepseek_r1"});
        });
        let parsers = launch(&effective, dir.path());
        assert_eq!(parsers.tool_call.name, None, "{engine}");
        assert_eq!(parsers.tool_call.source, ParserSource::Off);
        assert_eq!(parsers.reasoning.name.as_deref(), Some("deepseek_r1"));
        assert_eq!(parsers.reasoning.source, ParserSource::Declared);
        // `auto` is the default, written out.
        let effective = resolve(engine, |d| {
            d["engine_config"][engine] = json!({"tool_call_parser": "auto"});
        });
        assert_eq!(
            launch(&effective, dir.path()).tool_call.name.as_deref(),
            Some("qwen3_coder")
        );
    }
    // A declared parser on an unknown family is still used.
    let llama = checkpoint(json!({"model_type": "llama"}), None);
    let effective = resolve("vllm", |d| {
        d["engine_config"]["vllm"] = json!({"tool_call_parser": "llama3_json"});
    });
    let parsers = launch(&effective, llama.path());
    assert_eq!(parsers.tool_call.name.as_deref(), Some("llama3_json"));
    assert!(vllm_auto_tool_choice(&parsers, &[], &[]));
}

// T14: the same option in `extra_args` (as recipes pass it today) or in the
// installation's host-fixed args wins over the family default, and capyctl
// renders nothing for it.
#[test]
fn extra_and_host_fixed_args_win_over_the_family_default() {
    let dir = qwen3_5();
    for engine in ["vllm", "sglang"] {
        let effective = resolve(engine, |d| {
            d["engine_config"]["accept_extra_args"] = true.into();
            d["engine_config"]["extra_args"] = json!([
                "--tool-call-parser",
                "qwen3_xml",
                "--reasoning-parser=qwen3"
            ]);
        });
        let parsers = launch(&effective, dir.path());
        assert_eq!(parsers.tool_call.name, None, "{engine}");
        assert_eq!(parsers.tool_call.source, ParserSource::ExtraArgs);
        assert_eq!(parsers.reasoning.source, ParserSource::ExtraArgs);
    }
    let (deployment, mut host) = on("vllm");
    host["runtime_profiles"]["local"]["args"] =
        json!(["--tool-call-parser", "hermes", "--enable-auto-tool-choice"]);
    let effective = resolve_effective(&deployment, &host).unwrap();
    let parsers = launch(&effective, dir.path());
    assert_eq!(parsers.tool_call.source, ParserSource::HostFixed);
    assert_eq!(parsers.reasoning.name.as_deref(), Some("qwen3"));
    assert!(!vllm_auto_tool_choice(
        &parsers,
        &effective.profile.args,
        &[]
    ));

    // vLLM: an extra `--enable-auto-tool-choice` is not rendered twice.
    let effective = resolve("vllm", |d| {
        d["engine_config"]["accept_extra_args"] = true.into();
        d["engine_config"]["extra_args"] = json!(["--enable-auto-tool-choice"]);
    });
    let parsers = launch(&effective, dir.path());
    assert_eq!(parsers.tool_call.name.as_deref(), Some("qwen3_coder"));
    assert!(!vllm_auto_tool_choice(
        &parsers,
        &[],
        effective.engine_config.extra_args()
    ));
}

// T14: a declared parser (or `none`) and the same option in extra or
// host-fixed args contradict each other and are refused at deploy time.
#[test]
fn a_declared_parser_and_the_same_extra_option_are_refused() {
    for engine in ["vllm", "sglang"] {
        for value in ["hermes", "none"] {
            let (mut deployment, host) = on(engine);
            deployment["engine_config"][engine] = json!({"tool_call_parser": value});
            deployment["engine_config"]["accept_extra_args"] = true.into();
            deployment["engine_config"]["extra_args"] = json!(["--tool-call-parser", "x"]);
            let error = resolve_effective(&deployment, &host).unwrap_err();
            assert_eq!(
                error.path,
                format!("engine_config.{engine}.tool_call_parser")
            );
            assert!(error.to_string().contains("--tool-call-parser"), "{error}");
        }
    }
    let (mut deployment, mut host) = on("vllm");
    host["runtime_profiles"]["local"]["args"] = json!(["--reasoning-parser", "qwen3"]);
    deployment["engine_config"]["vllm"] = json!({"reasoning_parser": "qwen3"});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("host-fixed"), "{error}");
}

// T03: a parser name is one engine token; TensorFold has no parser setting.
#[test]
fn parser_values_are_checked() {
    for bad in ["", "a b", "--x", "x;y"] {
        let (mut deployment, host) = on("vllm");
        deployment["engine_config"]["vllm"] = json!({"reasoning_parser": bad});
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "engine_config.vllm.reasoning_parser", "{bad}");
    }
    let (mut deployment, _) = on("vllm");
    deployment["engine_config"]["tensorfold"] = json!({"tool_call_parser": "auto"});
    let text = deployment.to_string();
    assert!(capyctl_config::parse_strict(capyctl_config::ConfigKind::Deployment, &text).is_err());
}

// T14: what a server shows for a checkpoint on a remote host: a declared or
// overridden choice is known, a family default is chosen by the host.
#[test]
fn a_remote_host_chooses_the_family_default() {
    let effective = resolve("sglang", |d| {
        d["engine_config"]["sglang"] = json!({"reasoning_parser": "none"});
    });
    let parsers = parsers_on_remote_host(&effective).unwrap();
    assert_eq!(parsers.tool_call.source, ParserSource::OnHost);
    assert_eq!(parsers.reasoning.source, ParserSource::Off);
}

// T14: the choice survives the stored revision, and an undeclared choice
// leaves the stored form unchanged.
#[test]
fn the_declared_choice_round_trips_through_the_snapshot() {
    let effective = resolve("vllm", |d| {
        d["engine_config"]["vllm"] = json!({"tool_call_parser": "none"});
    });
    let stored = serde_json::to_string(&effective).unwrap();
    let decoded = capyctl_config::effective::decode_effective_snapshot(&stored).unwrap();
    assert_eq!(decoded.engine_config, effective.engine_config);
    let plain = serde_json::to_value(resolve("vllm", |_| {})).unwrap();
    assert!(plain["engine_config"].get("tool_call_parser").is_none());
    assert!(plain["engine_config"].get("reasoning_parser").is_none());
}
