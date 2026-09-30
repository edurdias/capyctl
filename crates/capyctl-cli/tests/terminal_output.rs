//! ADR 0021: commands print text by default, even when piped, and JSON only
//! when asked; `--json` output is today's JSON result. CPU test; not
//! qualification.
mod support;

use std::os::unix::fs::PermissionsExt;

fn state() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn run(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    support::capyctl()
        .env("CAPYCTL_STATE_DIR", dir.join("s"))
        .env_remove("CAPYCTL_VLLM_BIN")
        .env_remove("CAPYCTL_SGLANG_BIN")
        .args(args)
        .output()
        .unwrap()
}

// T02 T03 (ADR 0021): a piped command prints its text view; `--json` prints
// the JSON result, one object on one line.
#[test]
fn init_prints_text_when_piped_and_json_on_request() {
    let dir = state();
    let a = dir.path().join("a.yaml");
    let text = run(
        dir.path(),
        &["init", "host", "--output", a.to_str().unwrap()],
    );
    assert!(text.status.success(), "{text:?}");
    let stdout = String::from_utf8(text.stdout).unwrap();
    assert!(
        stdout.starts_with(&format!("Wrote {}\n", a.display())),
        "{stdout}"
    );
    assert!(
        !stdout.lines().any(|line| line.starts_with('{')),
        "{stdout}"
    );

    let b = dir.path().join("b.yaml");
    let json = run(
        dir.path(),
        &["init", "host", "--output", b.to_str().unwrap(), "--json"],
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["initialized"], true);
    assert_eq!(String::from_utf8(json.stdout).unwrap().lines().count(), 1);
}

// T02 (ADR 0021): validate prints one sentence.
#[test]
fn validate_prints_a_sentence() {
    let dir = state();
    let file = dir.path().join("host.yaml");
    assert!(run(
        dir.path(),
        &["init", "host", "--output", file.to_str().unwrap()]
    )
    .status
    .success());
    let out = run(
        dir.path(),
        &["validate", "config", "--file", file.to_str().unwrap()],
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{} is a valid host document\n", file.display())
    );
}

// T02 (ADR 0021): a notice from shared code, printed by a command, is text on
// stderr when piped too, never a JSON object.
#[test]
fn command_notices_are_text_not_json() {
    let dir = state();
    let file = dir.path().join("host.yaml");
    assert!(run(
        dir.path(),
        &["init", "host", "--output", file.to_str().unwrap()]
    )
    .status
    .success());
    let other = dir.path().join("other");
    let out = run(
        dir.path(),
        &[
            "join",
            "host",
            "--config",
            file.to_str().unwrap(),
            "--state-dir",
            other.to_str().unwrap(),
            "--join-file",
            dir.path().join("missing.join").to_str().unwrap(),
        ],
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    let first = stderr.lines().next().unwrap_or_default();
    assert!(first.starts_with("notice: "), "{stderr}");
}
