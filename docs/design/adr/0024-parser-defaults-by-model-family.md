# ADR 0024 — Tool-call and reasoning parsers by model family

**Status:** Accepted (owner decision 2026-10-03: "Yes, default by model family").
**Amends:** `SPEC.md` §10 (the 2026-09-24 note on tool calls) and ADR 0014 §2 (two
typed fields in the `vllm` and `sglang` blocks).
**Related:** ADR 0014 §5 (the fitted context, chosen the same way), ADR 0011 (the
user owns whether a recipe works).

## Context

CapyCTL relays tool calls and reasoning; the engine parses them. vLLM and SGLang do
so only when started with a tool-call parser and a reasoning parser. Without them a
tool call comes back as plain text in `content`, and the thinking trace stays in
`content` too. Until now a deployment had to pass them by hand through
`extra_args` with `accept_extra_args: true`: vLLM `--tool-call-parser qwen3_coder
--reasoning-parser qwen3 --enable-auto-tool-choice`, SGLang `--tool-call-parser
qwen3_coder --reasoning-parser qwen3`. Every published recipe for a Qwen model does.

## Decision

### 1. Detection

For vLLM and SGLang, CapyCTL reads the checkpoint's `config.json` (`model_type`, or
`architectures` when there is no `model_type`) and its chat template
(`tokenizer_config.json` `chat_template`, `chat_template.jinja`,
`chat_template.json`) where the checkpoint is read for the launch: on the embedded
host for standalone and on the host agent for a remote host, like the fitted
context. Nothing is added to the effective configuration.

| Family | `model_type` | Tool calls, vLLM | Tool calls, SGLang | Reasoning |
|---|---|---|---|---|
| Qwen3 | `qwen3`, `qwen3_moe` | `hermes` | `qwen25` | `qwen3` |
| Qwen3.5 (Qwen3.5, Qwen3.6, Qwen3.8) | `qwen3_5`, `qwen3_5_moe` | `qwen3_coder` | `qwen3_coder` | `qwen3` |

The template says which markup the model emits. In either family, `<function=` (the
XML tool-call format) chooses `qwen3_coder` (Qwen3-Coder is a `qwen3_moe`
checkpoint), JSON inside `<tool_call>` chooses the engine's JSON parser, and a
template with neither gets no tool parser. A template without `<think>` (an
instruct-only checkpoint) gets no reasoning parser. An unknown family, an
unreadable `config.json`, or no chat template means no parser: today's behaviour.

On vLLM, `--enable-auto-tool-choice` is rendered beside a tool parser CapyCTL
renders, unless the host-fixed or extra arguments already pass it.

Every parser name above is registered in vLLM 0.29.0 and 0.30.0 and SGLang 0.5.20
and 0.5.21, checked in the installed builds on 2026-10-03. Adding a verified engine
version rechecks them.

### 2. Deployment setting

`engine_config.vllm.tool_call_parser`, `engine_config.vllm.reasoning_parser`,
`engine_config.sglang.tool_call_parser` and `engine_config.sglang.reasoning_parser`
take `auto` (the default), `none` (no parser), or a parser name, passed as the
engine spells it (ADR 0011: CapyCTL checks the token, the engine checks the name).
TensorFold handles tool calls itself and has no such field.

A deployment file is YAML only, like every other deployment field: the three-ways
rule (flag, environment, YAML) covers role documents, and a deployment is a document
the server stores and resolves, not a role setting.

### 3. Extra and host-fixed arguments

The same option in `extra_args` or in the installation's host-fixed arguments, in
any spelling the engines accept (exact, `=value`, underscores, an abbreviation),
wins over `auto`: CapyCTL renders nothing for that parser, so existing recipes keep
working unchanged. A named parser or `none` beside the same option is a
contradiction and refused at resolution, naming the field and the option. The SGLang
entry rechecks that a chosen parser and the same extra never both reach the engine.

### 4. Status

`capyctl status deployment` and `capyctl validate config` show the parsers and
where each came from: `declared`, `off`, `extra_args`, `host_fixed`,
`model_family`, `unsupported` (the template has no such markup), `unknown_family`,
or `on_host` when a remote host chooses at launch.

## SPEC amendment (exact text)

- **§10**, the 2026-09-24 note, replace "A deployment serves `tool_choice: auto`
  (and SGLang any tool call) only when its engine is launched with its tool parser
  through `engine_config.extra_args` with `accept_extra_args: true` (vLLM
  `--enable-auto-tool-choice --tool-call-parser <name>`, SGLang `--tool-call-parser
  <name>`)." with "A deployment serves `tool_choice: auto` (and SGLang any tool call)
  only when its engine is launched with its tool parser. For a known model family
  CapyCTL chooses the tool-call and reasoning parsers itself (ADR 0024); otherwise
  the deployment names them in `engine_config.<engine>.tool_call_parser` and
  `reasoning_parser`, or passes them in `extra_args`."

## Consequences

- A Qwen3, Qwen3.5, Qwen3.6 or Qwen3.8 deployment returns structured `tool_calls`
  and a separate `reasoning_content` with no extra arguments.
- A launch whose parsers change re-renders the vLLM argument vector and the SGLang
  settings object; their digests cover the chosen names. A deployment that chose
  nothing renders exactly as before.
- CPU tests cover detection, the setting, the conflict rule and status. They are not
  qualification; the live check is recorded in the status runbook.
