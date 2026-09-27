//! Prints the website hero's `mllm list deployments` table
//! (`site/scripts/gen-hero.mjs`) with the same table code the CLI uses.
//! Website spec, Landing page 1: the output must match the real CLI format.
//!
//! Usage: `hero_table <deployments.json> <hosts.json>`, where the first file
//! is a `list deployments` JSON result and the second a host inventory.

use mllm_cli::table::{host_names, render, View};

fn read(path: Option<String>) -> serde_json::Value {
    let path = path.expect("usage: hero_table <deployments.json> <hosts.json>");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let deployments = read(args.next());
    let hosts = read(args.next());
    print!(
        "{}",
        render(View::Deployments, &deployments, &host_names(&hosts))
    );
}
