//! ADR 0018 §2 (review decision 2026-09-25): a role resolves its role
//! document and its engines file by the same rule as `capyctl engine`:
//! `--config`, else `$CAPYCTL_CONFIG`, names the document and the engines file
//! sits beside it; with neither, the engines file is
//! `<config home>/capyctl/engines.yaml`. So `capyctl engine` and the running role
//! always agree on the file.
//!
//! Each case makes the engines file the role should read a directory, so the
//! role refuses at load naming that path, before it binds a listener or
//! launches anything. CPU tests; not qualification.
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

fn private_dir() -> tempfile::TempDir {
    // Role custody rejects group-writable ancestors such as a shared /tmp.
    let directory = tempfile::Builder::new()
        .prefix("capyctl-role-config-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn mkdir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// `capyctl start <role>` with a clean environment: `HOME` and the state
/// directory under `root`, plus `extra`.
fn start(role: &str, root: &Path, extra: &[(&str, &Path)]) -> Output {
    let mut command = support::capyctl();
    command
        .args(["start", role])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("CAPYCTL_STATE_DIR", root.join("state"));
    support::pin_host_memory(&mut command);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn refused_naming(output: &Output, path: &Path) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains(&path.display().to_string()),
        "expected {} in: {text}",
        path.display()
    );
}

// T01
#[test]
fn a_host_named_by_capyctl_config_reads_the_engines_file_beside_it() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    mkdir(&root.path().join("state"));
    let dir = root.path().join("etc");
    mkdir(&dir);
    let document = dir.join("x.yaml");
    std::fs::write(
        &document,
        capyctl_config::remote_roles::HostConfig::template(&root.path().join("state")),
    )
    .unwrap();
    let engines = dir.join("engines.yaml");
    mkdir(&engines);
    let output = start("host", root.path(), &[("CAPYCTL_CONFIG", &document)]);
    refused_naming(&output, &engines);
}

// T01
#[test]
fn a_host_without_a_named_document_reads_the_config_home_engines_file() {
    let root = private_dir();
    let state = root.path().join("state");
    mkdir(&root.path().join("home"));
    mkdir(&state.join("config"));
    // The implicit host document, as `init host` writes it.
    std::fs::write(
        state.join("config/host.yaml"),
        capyctl_config::remote_roles::HostConfig::template(&state),
    )
    .unwrap();
    let config_home = root.path().join("xdg");
    let engines = config_home.join("capyctl/engines.yaml");
    mkdir(&engines);
    let output = start("host", root.path(), &[("XDG_CONFIG_HOME", &config_home)]);
    refused_naming(&output, &engines);
}

// T01
#[test]
fn a_standalone_named_by_capyctl_config_reads_the_engines_file_beside_it() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    mkdir(&root.path().join("state"));
    let dir = root.path().join("etc");
    mkdir(&dir);
    let document = dir.join("x.yaml");
    std::fs::write(
        &document,
        "schema_version: 1\nkind: standalone\nname: local\nhost:\n  name: local\n  runtime_profiles: {}\n",
    )
    .unwrap();
    let engines = dir.join("engines.yaml");
    mkdir(&engines);
    let output = start("standalone", root.path(), &[("CAPYCTL_CONFIG", &document)]);
    refused_naming(&output, &engines);
}
