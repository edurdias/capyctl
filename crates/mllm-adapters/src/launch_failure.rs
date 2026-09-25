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
/// parser's `argument --name: ...` and `unrecognized arguments: --name ...`.
/// Values that follow them are never read.
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

/// The bounded summary of a launch whose engine exited before readiness.
pub fn summary(engine_output: &str, exit: Option<EngineExit>) -> String {
    let mut text = String::from("the engine exited before readiness");
    match exit {
        Some(EngineExit::Code(code)) => text.push_str(&format!(" with exit code {code}")),
        Some(EngineExit::Signal(signal)) => text.push_str(&format!(" on signal {signal}")),
        None => {}
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
}
