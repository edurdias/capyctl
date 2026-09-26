//! Final review I14: CLI tests never read the developer's home. Twelve
//! binary tests resolved `engines.yaml` through the real `HOME` and
//! `XDG_CONFIG_HOME`, so a profile registered on the machine could change or
//! break them, and a green run did not prove what it claimed.
mod support;

use serde_json::Value;

// T03: every spawned `mllm` goes through `support::mllm()`, which points
// `HOME` and the XDG directories at an isolated home.
#[test]
fn every_test_spawns_the_binary_through_the_isolating_helper() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut direct = Vec::new();
    for entry in std::fs::read_dir(&tests).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let needle = concat!("env!(\"CARGO_BIN_", "EXE_mllm\")");
        if text.contains(needle) {
            direct.push(path.display().to_string());
        }
    }
    assert!(
        direct.is_empty(),
        "spawn the binary with support::mllm(), not directly: {direct:?}"
    );
}

// T03: a spawned role or client resolves every home-derived default (the
// state root, the models directory, the registered engines) under the
// isolated home, never the real one.
#[test]
fn a_spawned_mllm_sees_only_the_isolated_home() {
    let home = support::isolated_home();
    let real = std::env::var("HOME").unwrap();
    let out = support::mllm()
        .env_remove("MLLM_STATE_DIR")
        .args(["config", "show", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    let value = |path: &str| {
        shown["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|setting| setting["path"] == path)
            .map(|setting| setting["value"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|| panic!("{path} in {shown}"))
    };
    for path in ["state_dir", "host.model_store.path"] {
        let shown = value(path);
        assert!(
            std::path::Path::new(&shown).starts_with(home),
            "{path} = {shown}, not under the isolated home {}",
            home.display()
        );
        assert_ne!(
            std::path::Path::new(&shown),
            std::path::Path::new(&real).join(".local/state/mllm")
        );
    }
}
