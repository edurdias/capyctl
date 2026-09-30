//! Prints the CLI reference as Markdown for the website build
//! (`site/scripts/gen-cli.mjs`). Website spec, Docs.

fn main() {
    let options = clap_markdown::MarkdownOptions::new()
        .show_footer(false)
        .show_table_of_contents(false);
    print!(
        "{}",
        clap_markdown::help_markdown_command_custom(&capyctl_cli::grammar::command(), &options)
    );
}
