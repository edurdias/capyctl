//! Website spec, Landing page 1: the hero shows the `capyctl list deployments`
//! table (the CLI's default output) for two ready models and one parked model
//! on two generic hosts. The site renders it with this crate's table code
//! (`examples/hero_table.rs`); this test pins what that rendering shows.

use capyctl_cli::table::{host_names, render, View};
use serde_json::Value;

const DEPLOYMENTS: &str = include_str!("../../../site/src/data/hero-deployments.json");
const HOSTS: &str = include_str!("../../../site/src/data/hero-hosts.json");

fn hero() -> String {
    let deployments: Value = serde_json::from_str(DEPLOYMENTS).unwrap();
    let hosts: Value = serde_json::from_str(HOSTS).unwrap();
    render(View::Deployments, &deployments, &host_names(&hosts))
}

#[test]
fn hero_table_has_two_ready_one_parked_on_generic_hosts() {
    let table = hero();
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 4, "{table}");
    let header: Vec<&str> = lines[0].split_whitespace().collect();
    assert_eq!(header.first(), Some(&"NAME"));
    let state = header
        .iter()
        .position(|h| *h == "STATE")
        .expect("STATE column");
    let states: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.split_whitespace().nth(state).unwrap())
        .collect();
    assert_eq!(states, ["ready", "ready", "parked"]);
    assert!(
        table.contains("gpu-box") && table.contains("workstation"),
        "{table}"
    );
}
