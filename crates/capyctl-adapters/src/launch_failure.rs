//! SPEC §§6.4, 13.2: what a launch whose engine exited before readiness is
//! reported as outside the host's private log.
//!
//! The summary names what happened (the engine exited before readiness), the
//! exit code or signal when the host reaped it, and the option names the
//! engine's own output says it refused. It never carries an option value, a
//! path, a credential or any other engine output, so it may travel to the
//! controller and into status.

/// The longest summary (the wire bound on a host's launch failure).
pub const MAX_SUMMARY_BYTES: usize = 256;

/// At most this many refused option names are named.
const MAX_OPTIONS: usize = 4;

/// How the engine process ended, when the host that reaped it knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineExit {
    Code(i32),
    Signal(i32),
}

/// An option name as the summary may name it: `--` then 1 to 64 of
/// `[a-z0-9_-]`, starting with a letter or digit.
fn option_name(token: &str) -> Option<String> {
    let token =
        token.trim_matches(|c: char| matches!(c, '\'' | '"' | ',' | ':' | '(' | ')' | '[' | ']'));
    let token = token.split('=').next()?;
    let name = token.strip_prefix("--")?;
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    valid.then(|| format!("--{name}"))
}

/// The option names the engine's own output says it refused: an argument
/// parser's `argument --name: ...` and `unrecognized arguments: --name ...`,
/// and llama-server's `error: invalid argument: --name` (ADR 0029 §6,
/// `common/arg.cpp`). Values that follow them are never read.
pub fn rejected_options(engine_output: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if names.len() < MAX_OPTIONS && !names.contains(&name) {
            names.push(name);
        }
    };
    for line in engine_output.lines() {
        if let Some((_, rest)) = line.split_once("unrecognized arguments:") {
            rest.split_whitespace()
                .filter_map(option_name)
                .for_each(&mut push);
        }
        if let Some(name) = line
            .split_once("invalid argument:")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(option_name)
        {
            push(name);
        }
        let mut rest = line;
        while let Some(at) = rest.find("argument ") {
            rest = &rest[at + "argument ".len()..];
            // `argument --a/-a: invalid choice`: the first long form names it.
            let head = rest.split([':', ' ']).next().unwrap_or("");
            if let Some(name) = head.split('/').find_map(option_name) {
                push(name);
            }
        }
    }
    names
}

/// Lines of engine log quoted when a launch fails.
const LOG_TAIL_LINES: usize = 20;

/// The most log bytes read for a tail. An engine that logged a gigabyte before
/// dying must not be read into memory to explain itself.
const LOG_TAIL_BYTES: usize = 64 * 1024;

/// The last lines of the engine's log, bounded and redacted. The tail is the
/// engine's own account of why it left, and it is quoted into an error that
/// reaches a journal, so it is passed through redaction first (Spec §3).
pub fn log_tail(path: Option<&str>) -> String {
    let Some(path) = path else {
        return "(no engine log was configured for this launch)".into();
    };
    let text = match crate::engine_log::tail_bytes(std::path::Path::new(path), LOG_TAIL_BYTES) {
        Ok((bytes, _)) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return format!("(engine log {path} could not be read: {e})"),
    };
    let tail: Vec<&str> = text
        .lines()
        .rev()
        .take(LOG_TAIL_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        return format!("(engine log {path} is empty)");
    }
    crate::engine_log::LogRedactor::new().redact(&tail.join("\n"))
}

/// The bounded summary of a launch whose engine exited before readiness.
pub fn summary(engine_output: &str, exit: Option<EngineExit>) -> String {
    let mut text = String::from("the engine exited before readiness");
    match exit {
        Some(EngineExit::Code(code)) => text.push_str(&format!(" with exit code {code}")),
        Some(EngineExit::Signal(signal)) => text.push_str(&format!(" on signal {signal}")),
        None => {}
    }
    // ADR 0023 §3: TensorFold refuses a family that needs a drafter when it
    // has none (Qwen3.8 dense on CUDA); its line names a repository, so the
    // summary names the fix in fixed words instead.
    if engine_output.lines().any(|line| {
        line.contains("drafts with")
            && line.contains("which is not here")
            && line.contains("--no-drafts")
    }) {
        text.push_str(
            "; TensorFold needs a drafter for this model: name one with --drafter, \
             or add --no-drafts to turn drafts off",
        );
        return text;
    }
    // ADR 0023 §4 (amended 2026-10-03): TensorFold sizes one window and its
    // streams' drafter buffers inside the cap CapyCTL passes (the declared
    // Ready allocation) and refuses an explicit context that does not fit.
    if engine_output
        .lines()
        .any(|line| line.contains("CUDA startup memory budget cannot fit"))
    {
        text.push_str(
            "; TensorFold's memory cap cannot hold context_length beside its streams: \
             lower max_concurrent_requests or context_length, or raise the ready \
             allocation in resources",
        );
        return text;
    }
    // Name as many refused options as the bound allows.
    let mut options = rejected_options(engine_output);
    while !options.is_empty() {
        let named = format!(
            "{text}; it rejected argument{} {}",
            if options.len() > 1 { "s" } else { "" },
            options.join(", ")
        );
        if named.len() <= MAX_SUMMARY_BYTES {
            return named;
        }
        options.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // T20 T29: the summary names the refused option, never its value, and
    // the exit status when known.
    #[test]
    fn the_summary_names_the_rejected_option_never_its_value() {
        let output = "INFO starting\nvllm serve: error: argument --moe-backend: invalid choice: 'bogus-m53' (choose from 'a', 'b')\n";
        let text = summary(output, Some(EngineExit::Code(2)));
        assert_eq!(
            text,
            "the engine exited before readiness with exit code 2; it rejected argument --moe-backend"
        );
        assert!(!text.contains("bogus"));
        let text = summary(
            "error: unrecognized arguments: --foo=secret --bar value",
            None,
        );
        assert_eq!(
            text,
            "the engine exited before readiness; it rejected arguments --foo, --bar"
        );
        assert!(!text.contains("secret"));
        assert_eq!(
            summary("Traceback: CUDA out of memory", Some(EngineExit::Signal(9))),
            "the engine exited before readiness on signal 9"
        );
    }

    // T29: a token that is not a plain option name is never quoted.
    #[test]
    fn only_plain_option_names_are_named() {
        assert!(rejected_options("argument --API_KEY=abc: bad").is_empty());
        assert!(rejected_options("argument ../etc/passwd: bad").is_empty());
        assert_eq!(rejected_options("argument --tp/-t: bad"), vec!["--tp"]);
        let many = "unrecognized arguments: --a1 --a2 --a3 --a4 --a5";
        assert_eq!(rejected_options(many).len(), MAX_OPTIONS);
        let long = format!(
            "unrecognized arguments: --{} --{} --{} --{}",
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64)
        );
        let text = summary(&long, Some(EngineExit::Code(i32::MIN)));
        assert!(text.len() <= MAX_SUMMARY_BYTES, "{text}");
        assert!(text.contains(&"a".repeat(64)));
    }

    // T42 T29 (ADR 0029 §6): llama-server's parser names the refused option
    // after `invalid argument:`, or quoted after `error while handling
    // argument`; never its value.
    #[test]
    fn llama_server_rejections_name_the_option() {
        assert_eq!(
            rejected_options("error: invalid argument: --bogus_flag"),
            vec!["--bogus_flag"]
        );
        assert_eq!(
            rejected_options(
                "error while handling argument \"--spec-type\": unknown type secret-value"
            ),
            vec!["--spec-type"]
        );
        assert!(rejected_options("error: invalid argument: /etc/x").is_empty());
    }

    // T20 T29 (ADR 0023 §3): TensorFold's refusal to start a model that needs
    // a drafter is named with the fix, never with the repository or a path.
    #[test]
    fn a_tensorfold_drafter_refusal_names_the_fix() {
        let output = "[tensorfold] precision: checkpoint\ntensorfold: Qwen3.8 dense's CUDA engine drafts with z-lab/Qwen3.8-27B-DFlash2, which is not here: without it every round would decode one token. Run `tensorfold pull z-lab/Qwen3.8-27B-DFlash2` once (on both machines for --tp 2), or pass --no-drafts for the serial reference\n";
        let text = summary(output, Some(EngineExit::Code(1)));
        assert_eq!(
            text,
            "the engine exited before readiness with exit code 1; TensorFold needs a drafter \
             for this model: name one with --drafter, or add --no-drafts to turn drafts off"
        );
        assert!(!text.contains("z-lab"));
        assert!(text.len() <= MAX_SUMMARY_BYTES);
    }
    // ADR 0023 §4 (amended 2026-10-03): TensorFold refuses at start a context
    // its memory budget (the declared Ready allocation) cannot hold beside its
    // streams; the summary names the settings that fix it, never a number
    // from the engine's line.
    #[test]
    fn a_tensorfold_budget_refusal_names_the_fix() {
        let output = "[tensorfold] precision: checkpoint\nValueError: CUDA startup memory budget cannot fit requested context 32768; estimated largest fitting prompt-plus-reply window: 20480 tokens across the ranks. Use --context 20480 with a smaller prompt/reply reserve, or free memory or use smaller/quantized weights; no model weights or KV caches have been loaded. KV precision is unchanged.\n";
        let text = summary(output, Some(EngineExit::Code(1)));
        assert_eq!(
            text,
            "the engine exited before readiness with exit code 1; TensorFold's memory cap \
             cannot hold context_length beside its streams: lower max_concurrent_requests or \
             context_length, or raise the ready allocation in resources"
        );
        assert!(!text.contains("20480"));
        assert!(text.len() <= MAX_SUMMARY_BYTES);
    }
}
