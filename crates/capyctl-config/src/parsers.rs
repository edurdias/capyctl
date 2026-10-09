//! ADR 0024 (owner decision 2026-10-03): tool-call and reasoning parsers
//! chosen by model family for vLLM and SGLang.
//!
//! Without a tool-call parser the engine returns a tool call as plain text,
//! and without a reasoning parser the thinking trace stays in `content`.
//! CapyCTL relays and never parses (SPEC §10 note), so the engine has to be
//! started with both. For a known model family capyctl now chooses them from
//! the checkpoint's `config.json` (`model_type`, else `architectures`) and its
//! chat template, which says what markup the model emits:
//!
//! | Family | `model_type` | tool calls (vLLM / SGLang) | reasoning |
//! |---|---|---|---|
//! | Qwen3 | `qwen3`, `qwen3_moe` | `hermes` / `qwen25` | `qwen3` |
//! | Qwen3.5 (also Qwen3.6, Qwen3.8) | `qwen3_5`, `qwen3_5_moe` | `qwen3_coder` | `qwen3` |
//!
//! The template picks the tool-call format: the XML markup (`<function=`)
//! takes `qwen3_coder` in either family (Qwen3-Coder is a `qwen3_moe`
//! checkpoint), JSON in `<tool_call>` takes the JSON parser, and a template
//! with neither gets no tool parser. One with no `<think>` gets no reasoning
//! parser. Every name above is registered in vLLM 0.29.0 and 0.30.0 and SGLang
//! 0.5.20 and 0.5.21 (checked in the installed builds, 2026-10-03).
//!
//! The deployment's `engine_config.<engine>.tool_call_parser` and
//! `reasoning_parser` take `auto` (the default), `none`, or a parser name. The
//! same option in the deployment's `extra_args` or the installation's
//! host-fixed args wins over `auto` and capyctl renders nothing for it; with a
//! name or `none` it is refused at resolution. TensorFold handles tool calls
//! itself and has no setting.
//!
//! Like the fitted context (`context_fit`), the choice is made where the
//! checkpoint is read, at launch render; it is not part of the effective
//! configuration.

use std::path::Path;

use capyctl_domain::launch::LaunchSettings;
use serde::Serialize;
use serde_json::Value;

use crate::engine_policy::{normalize_option_name, Engine};

/// The written default: capyctl chooses by model family.
pub const AUTO: &str = "auto";
/// Turns a parser off.
pub const OFF: &str = "none";

/// The engine options a parser setting renders (the same spelling in vLLM and
/// SGLang).
pub const TOOL_CALL_OPTION: &str = "--tool-call-parser";
pub const REASONING_OPTION: &str = "--reasoning-parser";
/// vLLM serves `tool_choice: auto` only with this switch beside its parser.
pub const VLLM_AUTO_TOOL_CHOICE: &str = "--enable-auto-tool-choice";

/// The largest template file read. Tokenizer configurations are kilobytes.
const MAX_TEMPLATE_BYTES: u64 = 4 << 20;

/// Where a parser choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParserSource {
    /// The deployment named the parser.
    Declared,
    /// The deployment turned it off (`none`).
    Off,
    /// The deployment's `extra_args` set the option; capyctl passes nothing.
    ExtraArgs,
    /// The installation's host-fixed args set it; capyctl passes nothing.
    HostFixed,
    /// Chosen from the model family and the chat template.
    ModelFamily,
    /// A known family whose chat template does not use this markup.
    Unsupported,
    /// No known family, or the checkpoint could not be read: no parser.
    UnknownFamily,
    /// The checkpoint is on a remote host, which chooses at launch.
    OnHost,
}

/// One parser of one launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParserChoice {
    /// The parser capyctl passes to the engine; `None` passes nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source: ParserSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The parsers of one vLLM or SGLang launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Parsers {
    pub tool_call: ParserChoice,
    pub reasoning: ParserChoice,
    /// The recognised family (its `model_type`), when one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
}

#[derive(Clone, Copy)]
enum Kind {
    ToolCall,
    Reasoning,
}

impl Kind {
    fn option(self) -> &'static str {
        match self {
            Kind::ToolCall => TOOL_CALL_OPTION,
            Kind::Reasoning => REASONING_OPTION,
        }
    }
}

struct Family {
    name: &'static str,
    model_types: &'static [&'static str],
    architectures: &'static [&'static str],
    vllm_json_tools: &'static str,
    sglang_json_tools: &'static str,
}

const FAMILIES: &[Family] = &[
    Family {
        name: "qwen3",
        model_types: &["qwen3", "qwen3_moe"],
        architectures: &["Qwen3ForCausalLM", "Qwen3MoeForCausalLM"],
        vllm_json_tools: "hermes",
        sglang_json_tools: "qwen25",
    },
    Family {
        name: "qwen3_5",
        model_types: &["qwen3_5", "qwen3_5_moe"],
        architectures: &[
            "Qwen3_5ForConditionalGeneration",
            "Qwen3_5MoeForConditionalGeneration",
            "Qwen3_5ForCausalLM",
            "Qwen3_5MoeForCausalLM",
        ],
        vllm_json_tools: "hermes",
        sglang_json_tools: "qwen25",
    },
];

const XML_TOOLS: &str = "qwen3_coder";
const QWEN3_REASONING: &str = "qwen3";

/// Check a declared parser value: `auto`, `none` or one engine token.
pub fn valid_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Whether `args` set `option`, in any spelling the engines' parsers accept:
/// exact, `=value`, underscores, or an abbreviation (both parsers expand an
/// unambiguous prefix).
pub fn args_set(args: &[String], option: &str) -> bool {
    args.iter()
        .filter(|token| token.starts_with("--"))
        .map(|token| normalize_option_name(token))
        .any(|name| name == option || (name.len() > 2 && option.starts_with(name.as_str())))
}

fn declared(settings: &LaunchSettings, kind: Kind) -> Option<Option<&str>> {
    let (tool, reasoning) = match settings {
        LaunchSettings::Vllm(s) => (&s.tool_call_parser, &s.reasoning_parser),
        LaunchSettings::Sglang(s) => (&s.tool_call_parser, &s.reasoning_parser),
        // ADR 0029 §12: llama-server derives its parsers from the chat template.
        LaunchSettings::Tensorfold(_) | LaunchSettings::Llamacpp(_) => return None,
    };
    Some(match kind {
        Kind::ToolCall => tool.as_deref(),
        Kind::Reasoning => reasoning.as_deref(),
    })
}

fn fixed(source: ParserSource, reason: Option<String>) -> ParserChoice {
    ParserChoice {
        name: None,
        source,
        reason,
    }
}

/// The choice that does not depend on the checkpoint, or `None` for `auto`.
fn without_checkpoint(
    settings: &LaunchSettings,
    profile_args: &[String],
    kind: Kind,
) -> Option<ParserChoice> {
    let option = kind.option();
    if args_set(profile_args, option) {
        return Some(fixed(
            ParserSource::HostFixed,
            Some(format!("the installation's host-fixed args set `{option}`")),
        ));
    }
    if args_set(settings.extra_args(), option) {
        return Some(fixed(
            ParserSource::ExtraArgs,
            Some(format!("the deployment's extra_args set `{option}`")),
        ));
    }
    match declared(settings, kind).flatten() {
        None => None,
        Some(OFF) => Some(fixed(ParserSource::Off, None)),
        Some(name) => Some(ParserChoice {
            name: Some(name.to_owned()),
            source: ParserSource::Declared,
            reason: None,
        }),
    }
}

fn family_of(config: &Value) -> Result<&'static Family, String> {
    let model_type = config.get("model_type").and_then(Value::as_str);
    let architectures: Vec<&str> = config
        .get("architectures")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let found = match model_type {
        Some(model_type) => FAMILIES
            .iter()
            .find(|family| family.model_types.contains(&model_type)),
        None => FAMILIES.iter().find(|family| {
            architectures
                .iter()
                .any(|name| family.architectures.contains(name))
        }),
    };
    found.ok_or_else(|| {
        let named = model_type
            .or_else(|| architectures.first().copied())
            .unwrap_or("unnamed");
        format!("model family `{named}` has no parser defaults")
    })
}

fn read_bounded(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_TEMPLATE_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= MAX_TEMPLATE_BYTES).then_some(text)
}

fn template_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        // A list of named templates (`default`, `tool_use`, ...).
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("template").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The checkpoint's chat template, from `tokenizer_config.json`,
/// `chat_template.jinja` or `chat_template.json`, in the order the engines'
/// tokenizers read them; every one found is searched.
pub fn read_chat_template(checkpoint_root: &Path) -> Option<String> {
    let mut text = String::new();
    for (file, key) in [
        ("tokenizer_config.json", Some("chat_template")),
        ("chat_template.jinja", None),
        ("chat_template.json", Some("chat_template")),
    ] {
        let Some(contents) = read_bounded(&checkpoint_root.join(file)) else {
            continue;
        };
        match key {
            None => text.push_str(&contents),
            Some(key) => {
                if let Ok(json) = serde_json::from_str::<Value>(&contents) {
                    text.push_str(&template_text(&json[key]));
                }
            }
        }
        text.push('\n');
    }
    (!text.trim().is_empty()).then_some(text)
}

fn from_family(
    engine: Engine,
    kind: Kind,
    family: &Family,
    template: Option<&str>,
) -> ParserChoice {
    let Some(template) = template else {
        return fixed(
            ParserSource::Unsupported,
            Some("the checkpoint has no chat template".into()),
        );
    };
    let name = match kind {
        Kind::ToolCall if !template.contains("<tool_call>") && !template.contains("<function=") => {
            return fixed(
                ParserSource::Unsupported,
                Some("the chat template has no tool-call markup".into()),
            );
        }
        Kind::ToolCall if template.contains("<function=") => XML_TOOLS,
        Kind::ToolCall => match engine {
            Engine::Sglang => family.sglang_json_tools,
            _ => family.vllm_json_tools,
        },
        Kind::Reasoning if !template.contains("<think>") => {
            return fixed(
                ParserSource::Unsupported,
                Some("the chat template has no thinking block".into()),
            );
        }
        Kind::Reasoning => QWEN3_REASONING,
    };
    ParserChoice {
        name: Some(name.into()),
        source: ParserSource::ModelFamily,
        reason: Some(format!("model family {}", family.name)),
    }
}

/// The parsers of one launch of `settings` against the checkpoint at
/// `checkpoint_root`. `None` for TensorFold, which has no parser setting.
pub fn parsers_for_launch(
    engine: Engine,
    settings: &LaunchSettings,
    profile_args: &[String],
    checkpoint_root: Option<&Path>,
) -> Option<Parsers> {
    if !matches!(engine, Engine::Vllm | Engine::Sglang) {
        return None;
    }
    declared(settings, Kind::ToolCall)?;
    let tool = without_checkpoint(settings, profile_args, Kind::ToolCall);
    let reasoning = without_checkpoint(settings, profile_args, Kind::Reasoning);
    let family = if tool.is_none() || reasoning.is_none() {
        Some(
            checkpoint_root
                .ok_or_else(|| "the deployment resolves to no checkpoint directory".to_owned())
                .and_then(crate::context_fit::read_model_config)
                .and_then(|config| family_of(&config)),
        )
    } else {
        None
    };
    let template = match (&family, checkpoint_root) {
        (Some(Ok(_)), Some(root)) => read_chat_template(root),
        _ => None,
    };
    let choose = |given: Option<ParserChoice>, kind: Kind| {
        given.unwrap_or_else(|| match &family {
            Some(Ok(family)) => from_family(engine, kind, family, template.as_deref()),
            Some(Err(reason)) => fixed(ParserSource::UnknownFamily, Some(reason.clone())),
            None => unreachable!("the family is read whenever a choice is auto"),
        })
    };
    Some(Parsers {
        tool_call: choose(tool, Kind::ToolCall),
        reasoning: choose(reasoning, Kind::Reasoning),
        family: match family {
            Some(Ok(family)) => Some(family.name.into()),
            _ => None,
        },
    })
}

/// The parsers of the effective deployment, reading its checkpoint where this
/// machine sees it.
pub fn parsers_for_effective(effective: &crate::effective::EffectiveDeployment) -> Option<Parsers> {
    parsers_for_launch(
        effective.profile.engine,
        &effective.engine_config,
        &effective.profile.args,
        effective.model.resolved_path.as_deref().map(Path::new),
    )
}

/// The parsers as a server sees a deployment whose checkpoint is on a remote
/// host: a declared or overridden choice is known here; `auto` is chosen by
/// the host from its own copy at launch.
pub fn parsers_on_remote_host(
    effective: &crate::effective::EffectiveDeployment,
) -> Option<Parsers> {
    let settings = &effective.engine_config;
    declared(settings, Kind::ToolCall)?;
    let on_host = || {
        fixed(
            ParserSource::OnHost,
            Some("chosen by the host from its checkpoint at launch".into()),
        )
    };
    let args = &effective.profile.args;
    Some(Parsers {
        tool_call: without_checkpoint(settings, args, Kind::ToolCall).unwrap_or_else(on_host),
        reasoning: without_checkpoint(settings, args, Kind::Reasoning).unwrap_or_else(on_host),
        family: None,
    })
}

/// vLLM serves `tool_choice: auto` only with `--enable-auto-tool-choice`.
/// capyctl renders it beside a tool parser it renders, unless the host-fixed
/// or extra args already pass it.
pub fn vllm_auto_tool_choice(
    parsers: &Parsers,
    profile_args: &[String],
    extra_args: &[String],
) -> bool {
    parsers.tool_call.name.is_some()
        && !args_set(profile_args, VLLM_AUTO_TOOL_CHOICE)
        && !args_set(extra_args, VLLM_AUTO_TOOL_CHOICE)
}
