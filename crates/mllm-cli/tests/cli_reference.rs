//! Website spec, Docs: the CLI reference is generated from the clap
//! definitions, never hand-written. These tests keep that generator honest.

use mllm_cli::grammar;

#[test]
fn clap_definition_is_internally_consistent() {
    grammar::command().debug_assert();
}

#[test]
fn generated_reference_covers_every_subcommand() {
    let command = grammar::command();
    let markdown = clap_markdown::help_markdown_command_custom(
        &command,
        &clap_markdown::MarkdownOptions::new()
            .show_footer(false)
            .show_table_of_contents(false),
    );
    for sub in command.get_subcommands().filter(|s| !s.is_hide_set()) {
        let heading = format!("## `mllm {}`", sub.get_name());
        assert!(markdown.contains(&heading), "missing {heading}");
    }
    assert!(!markdown.contains("clap-markdown"), "footer must be off");
}
