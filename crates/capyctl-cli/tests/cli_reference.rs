//! Website spec, Docs: the CLI reference is generated from the clap
//! definitions, never hand-written. These tests keep that generator honest.

use capyctl_cli::grammar;

fn reference() -> String {
    clap_markdown::help_markdown_command_custom(
        &grammar::command(),
        &clap_markdown::MarkdownOptions::new()
            .show_footer(false)
            .show_table_of_contents(false),
    )
}

#[test]
fn clap_definition_is_internally_consistent() {
    grammar::command().debug_assert();
}

#[test]
fn generated_reference_covers_every_subcommand() {
    let command = grammar::command();
    let markdown = reference();
    for sub in command.get_subcommands().filter(|s| !s.is_hide_set()) {
        let heading = format!("## `capyctl {}`", sub.get_name());
        assert!(markdown.contains(&heading), "missing {heading}");
    }
    assert!(!markdown.contains("clap-markdown"), "footer must be off");
}

/// Users read the help text: it must not cite internal documents or use
/// internal vocabulary. Citations belong in `//` comments above the `///`.
#[test]
fn generated_reference_has_no_internal_terms() {
    let markdown = reference();
    for term in ["SPEC", "ADR ", "§", "wner decision"] {
        assert!(
            !markdown.contains(term),
            "the CLI reference contains {term:?}"
        );
    }
    const WORDS: [&str; 7] = [
        "lease",
        "leases",
        "fencing",
        "epoch",
        "epochs",
        "ledger",
        "reservation",
    ];
    for word in markdown.split(|c: char| !c.is_ascii_alphanumeric()) {
        assert!(
            !WORDS.contains(&word.to_ascii_lowercase().as_str()),
            "the CLI reference contains {word:?}"
        );
        // Acceptance-matrix ids such as T21.
        let is_matrix_id = word.len() == 3
            && word.starts_with('T')
            && word[1..].chars().all(|c| c.is_ascii_digit());
        assert!(!is_matrix_id, "the CLI reference contains {word}");
    }
}

/// Every visible command and subcommand says what it does.
#[test]
fn every_visible_command_has_a_description() {
    fn walk(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
        for sub in command.get_subcommands().filter(|s| !s.is_hide_set()) {
            let name = format!("{path} {}", sub.get_name());
            if sub.get_about().is_none() {
                missing.push(name.clone());
            }
            walk(sub, &name, missing);
        }
    }
    let mut missing = Vec::new();
    walk(&grammar::command(), "capyctl", &mut missing);
    assert!(missing.is_empty(), "no description: {missing:?}");
}
