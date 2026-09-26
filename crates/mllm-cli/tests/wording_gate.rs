//! Final review I13: user-facing text says what the product does, not how it
//! was decided. `--help` of every command carries no requirement citations
//! or process terms, and neither do the operator guides, examples and
//! service units a release ships. No tracked file names the review process
//! that produced a change ("controller ruling").

/// Terms that belong in code comments, the ADRs and the SPEC, never in help.
const HELP_FORBIDDEN: &[&str] = &[
    "§",
    "ADR ",
    "Owner decision",
    "owner decision",
    "Owner rule",
    "owner rule",
    "design §",
    "Design §",
    "ruling",
    "SPEC ",
];

/// Terms the shipped operator documents must not carry.
const DOCS_FORBIDDEN: &[&str] = &[
    "Owner decision",
    "owner decision",
    "design §",
    "Design §",
    "controller ruling",
    "Controller ruling",
];

fn repository() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tracked(paths: &[&str]) -> Vec<std::path::PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repository())
        .args(["ls-files", "--"])
        .args(paths)
        .output()
        .expect("git runs");
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| repository().join(line))
        .collect()
}

// T03
#[test]
fn help_text_carries_no_requirement_citations_or_process_terms() {
    let mut found = Vec::new();
    for (command, text) in mllm_cli::grammar::help_texts() {
        for term in HELP_FORBIDDEN {
            for line in text.lines().filter(|line| line.contains(term)) {
                found.push(format!("{command}: {term:?} in {line:?}"));
            }
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

// T03
#[test]
fn shipped_operator_documents_carry_no_process_terms() {
    let mut found = Vec::new();
    for path in tracked(&[
        "docs/operations",
        "docs/examples",
        "packaging/systemd",
        "README.md",
    ]) {
        let text = std::fs::read_to_string(&path).unwrap();
        for term in DOCS_FORBIDDEN {
            for (number, line) in text.lines().enumerate().filter(|(_, l)| l.contains(term)) {
                found.push(format!("{}:{}: {line}", path.display(), number + 1));
            }
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

// T03
#[test]
fn no_tracked_file_names_the_review_process() {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repository())
        .args(["grep", "-n", "-i", "controller ruling"])
        .output()
        .expect("git runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let found: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with("crates/mllm-cli/tests/wording_gate.rs"))
        .collect();
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// Terms runtime messages (warnings, errors, notices) must not carry.
const RUNTIME_FORBIDDEN: &[&str] = &["SPEC §", "ADR ", "owner decision", "Owner decision"];

/// The contents of every string literal in Rust `source`, with the line it
/// starts on. Comments (line, doc and block) are skipped, so a requirement
/// cited in a comment is fine; only text that can reach a user is scanned.
fn string_literals(source: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let mut found = Vec::new();
    let mut line = 1;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if c == 'r' && (next == Some('"') || next == Some('#')) && {
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            chars.get(j) == Some(&'"') && (i == 0 || !chars[i - 1].is_alphanumeric())
        } {
            let mut hashes = 0;
            i += 1;
            while chars[i] == '#' {
                hashes += 1;
                i += 1;
            }
            i += 1;
            let start = line;
            let mut text = String::new();
            while i < chars.len() {
                if chars[i] == '"' && (1..=hashes).all(|k| chars.get(i + k) == Some(&'#')) {
                    i += 1 + hashes;
                    break;
                }
                if chars[i] == '\n' {
                    line += 1;
                }
                text.push(chars[i]);
                i += 1;
            }
            found.push((start, text));
        } else if c == '"' {
            i += 1;
            let start = line;
            let mut text = String::new();
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    text.push(chars[i]);
                    i += 1;
                }
                if chars[i] == '\n' {
                    line += 1;
                }
                text.push(chars[i]);
                i += 1;
            }
            i += 1;
            found.push((start, text));
        } else if c == '\'' {
            // A char literal ('x', '\n', '"'); anything else is a lifetime.
            if next == Some('\\') {
                i += 2;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if chars.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    found
}

// T03: runtime output (warnings, refusals, validation errors) is written for
// the operator, in plain words. Requirement citations stay in comments.
#[test]
fn runtime_messages_carry_no_requirement_citations_or_process_terms() {
    let mut found = Vec::new();
    for path in tracked(&["crates/mllm-cli/src", "crates/mllm-config/src"]) {
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for (line, literal) in string_literals(&source) {
            for term in RUNTIME_FORBIDDEN {
                if literal.contains(term) {
                    found.push(format!(
                        "{}:{line}: {term:?} in {literal:?}",
                        path.display()
                    ));
                }
            }
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn the_literal_scanner_skips_comments_and_reads_strings() {
    let source = "// SPEC § in a comment\nlet a = \"plain\"; /* ADR 1 */ let b = '\"';\nlet c = r#\"x \"ADR 2\"\"#;\nlet d = \"a\\\"b\";\n";
    let literals: Vec<String> = string_literals(source)
        .into_iter()
        .map(|(_, s)| s)
        .collect();
    assert_eq!(literals, vec!["plain", "x \"ADR 2\"", "a\\\"b"]);
}
