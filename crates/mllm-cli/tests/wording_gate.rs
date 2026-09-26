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
