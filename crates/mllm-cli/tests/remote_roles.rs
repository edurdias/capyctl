use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

mod support;
use support::process::{free_ports, output_within, Guarded};

/// How long a role start that must refuse is given to exit. A refusal that
/// regressed into a serving role fails the test here instead of hanging it.
const REFUSAL_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// A role start that must refuse and exit, bounded by [`REFUSAL_BOUND`].
fn refused_start(root: &Path, args: &[&str]) -> std::process::Output {
    output_within(
        Command::new(env!("CARGO_BIN_EXE_mllm"))
            .args(args)
            .env("MLLM_STATE_DIR", root)
            .env_remove("MLLM_VLLM_BIN")
            .env_remove("MLLM_SGLANG_BIN"),
        REFUSAL_BOUND,
    )
}
fn root() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}
fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mllm"))
        .args(args)
        .env("MLLM_STATE_DIR", root)
        .env_remove("MLLM_VLLM_BIN")
        .env_remove("MLLM_SGLANG_BIN")
        .output()
        .unwrap()
}
// T02, T04: initialization protects identity and never overwrites existing files.
#[test]
fn initialize_server_and_host_without_engines_or_secret_output() {
    let temp = root();
    let server = temp.path().join("server.yaml");
    let out = cli(
        &temp.path().join("server-state"),
        &["init", "server", "--output", server.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let original = fs::read(&server).unwrap();
    assert_eq!(
        fs::metadata(&server).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("PRIVATE KEY"));
    let duplicate = cli(
        &temp.path().join("server-state"),
        &["init", "server", "--output", server.to_str().unwrap()],
    );
    assert!(!duplicate.status.success());
    assert_eq!(fs::read(&server).unwrap(), original);
    let host = temp.path().join("host.yaml");
    assert!(cli(
        &temp.path().join("host-state"),
        &["init", "host", "--output", host.to_str().unwrap()]
    )
    .status
    .success());
    let offline = refused_start(
        &temp.path().join("host-state"),
        &["start", "host", "--config", host.to_str().unwrap()],
    );
    assert!(!offline.status.success());
    assert!(String::from_utf8_lossy(&offline.stderr).contains("join host"));
}
// T04 (W12, U5 live): `join host --join-file NAME` with a bare relative name
// reads the invitation from the working directory. It used to fail before
// reading anything because the empty parent could not be canonicalized.
#[test]
fn join_reads_a_relative_invitation_from_the_working_directory() {
    let temp = root();
    let state = temp.path().join("host-state");
    let config = temp.path().join("host.yaml");
    assert!(cli(&state, &["init", "host", "--output", config.to_str().unwrap()])
        .status
        .success());
    let invitation = temp.path().join("host.join");
    fs::write(&invitation, b"not an invitation").unwrap();
    fs::set_permissions(&invitation, fs::Permissions::from_mode(0o600)).unwrap();
    let joined = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .current_dir(temp.path())
        .args([
            "join",
            "host",
            "--join-file",
            "host.join",
            "--config",
            config.to_str().unwrap(),
            "--output",
            "json",
        ])
        .env("MLLM_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(!joined.status.success());
    // The file was found and read; only its content is refused.
    let stderr = String::from_utf8_lossy(&joined.stderr);
    assert!(stderr.contains("Invalid join invitation"), "{stderr}");
}
// T03: invalid explicit config never generates state or falls back. SPEC
// §15.2 (R13): the standalone role honours `--config` the same way; a missing
// explicit document exits as invalid configuration (2).
#[test]
fn explicit_missing_role_config_has_no_side_effects() {
    let temp = root();
    let state = temp.path().join("state");
    for role in ["server", "host", "standalone"] {
        let result = refused_start(
            &state,
            &["start", role, "--config", "/definitely/missing/mllm.yaml"],
        );
        assert!(!result.status.success());
        assert!(!state.exists(), "{role}");
        if role == "standalone" {
            assert_eq!(result.status.code(), Some(2), "{result:?}");
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(stderr.contains("/definitely/missing/mllm.yaml"), "{stderr}");
            assert!(!stderr.contains("not_implemented"), "{stderr}");
        }
    }
}

// T04: competing initializers cannot replace a winner's configuration or keys.
#[test]
fn concurrent_initialization_has_one_winner_and_preserves_identity() {
    let temp = root();
    let state = temp.path().join("state");
    let output = temp.path().join("server.yaml");
    let start = || Command::new(env!("CARGO_BIN_EXE_mllm"))
        .args(["init","server","--output",output.to_str().unwrap()])
        .env("MLLM_STATE_DIR",&state).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut first = start();
    let mut second = start();
    assert_eq!(usize::from(first.wait().unwrap().success()) + usize::from(second.wait().unwrap().success()),1);
    let ca = fs::read(state.join("identity/controller-ca.json")).unwrap();
    assert!(!cli(&state,&["init","server","--output",output.to_str().unwrap()]).status.success());
    assert_eq!(fs::read(state.join("identity/controller-ca.json")).unwrap(),ca);
}
// T01, T37: host log retention is an explicit local option.
#[test]
fn host_debug_logging_requires_the_flag() {
    use mllm_cli::grammar::parse_invocation;
    assert!(
        !parse_invocation(["mllm", "start", "host"])
            .unwrap()
            .debug_engine_logs
    );
    assert!(
        parse_invocation(["mllm", "start", "host", "--debug-engine-logs"])
            .unwrap()
            .debug_engine_logs
    );
    assert!(parse_invocation(["mllm", "start", "server", "--debug-engine-logs"]).is_err());
}

/// A running role; dropping it kills its process group (support::process).
struct Service(Guarded);
impl Service {
    fn start(root: &Path, role: &str, config: &Path) -> Self {
        Self(Guarded::spawn(
            Command::new(env!("CARGO_BIN_EXE_mllm"))
                .args(["start", role, "--config", config.to_str().unwrap()])
                .env("MLLM_STATE_DIR", root)
                .env_remove("MLLM_VLLM_BIN")
                .env_remove("MLLM_SGLANG_BIN")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        ))
    }
    fn stop(&mut self) {
        self.0.signal(libc::SIGTERM);
        self.0
            .exit_within(std::time::Duration::from_secs(5), "the role");
    }
}
fn hosts(root: &Path, config: &Path, online: bool, expected: usize) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let out = cli(
            root,
            &[
                "list",
                "hosts",
                "--config",
                config.to_str().unwrap(),
                "--output",
                "json",
            ],
        );
        if out.status.success() {
            let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            if value["hosts"].as_array().is_some_and(|hosts| {
                hosts.len() == expected && hosts.iter().all(|h| h["online"] == online)
            }) {
                return value;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "role did not reach expected host connectivity: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
// T01–T07, T33, T34: real binary, TLS enrollment, independent roles and restart.
#[test]
fn product_enrolls_unprepared_host_and_reconnects_without_identity_change() {
    let temp = root();
    let server_root = temp.path().join("server");
    let host_root = temp.path().join("host");
    let server_config = temp.path().join("server.yaml");
    let host_config = temp.path().join("host.yaml");
    for (state, role, config) in [
        (&server_root, "server", &server_config),
        (&host_root, "host", &host_config),
    ] {
        let result = cli(state, &["init", role, "--output", config.to_str().unwrap()]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&server_config).unwrap()).unwrap();
    // Chosen below the ephemeral range, where no outbound connection of a
    // parallel test can take them before the server binds (support::process).
    let ports = free_ports(4, false);
    for (i, name) in ["management", "inference", "bootstrap", "control"]
        .iter()
        .enumerate()
    {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], ports[i]));
        document["listeners"][name]["bind"] = addr.to_string().into();
        if matches!(*name, "bootstrap" | "control") {
            document["enrollment"][format!("{name}_address")] = format!("https://{addr}").into();
        }
    }
    fs::write(&server_config, serde_json::to_vec(&document).unwrap()).unwrap();
    let mut server = Service::start(&server_root, "server", &server_config);
    hosts(&server_root, &server_config, false, 0);
    let invitation = temp.path().join("host.join");
    let invited = cli(
        &server_root,
        &[
            "invite",
            "host",
            "--name",
            "test-spark",
            "--config",
            server_config.to_str().unwrap(),
            "--output",
            invitation.to_str().unwrap(),
        ],
    );
    assert!(
        invited.status.success(),
        "{}",
        String::from_utf8_lossy(&invited.stderr)
    );
    // W12 (U5 live): an operator joins from the directory holding the
    // invitation, so `--join-file` is a bare relative name.
    assert!(invitation.parent() == Some(temp.path()));
    let joined = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .current_dir(temp.path())
        .args([
            "join",
            "host",
            "--join-file",
            "host.join",
            "--config",
            host_config.to_str().unwrap(),
        ])
        .env("MLLM_STATE_DIR", &host_root)
        .env_remove("MLLM_VLLM_BIN")
        .env_remove("MLLM_SGLANG_BIN")
        .output()
        .unwrap();
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    let identity = fs::read(host_root.join("identity/host-identity.json")).unwrap();
    let mut host = Service::start(&host_root, "host", &host_config);
    let snapshot = hosts(&server_root, &server_config, true, 1);
    assert_eq!(snapshot["hosts"][0]["eligible"], false);
    assert_eq!(
        snapshot["hosts"][0]["session"]["domains"][0]["kind"],
        "system"
    );
    assert_eq!(
        snapshot["hosts"][0]["session"]["profiles"],
        serde_json::json!([])
    );
    let id = snapshot["hosts"][0]["host_id"].clone();
    host.stop();
    hosts(&server_root, &server_config, false, 1);
    host = Service::start(&host_root, "host", &host_config);
    assert_eq!(
        hosts(&server_root, &server_config, true, 1)["hosts"][0]["host_id"],
        id
    );
    server.stop();
    server = Service::start(&server_root, "server", &server_config);
    assert_eq!(
        hosts(&server_root, &server_config, true, 1)["hosts"][0]["host_id"],
        id
    );
    assert_eq!(
        fs::read(host_root.join("identity/host-identity.json")).unwrap(),
        identity
    );
    host.stop();
    server.stop();
}

/// Initialize a server and a host, enroll the host through the product and
/// start the server. Returns the server and the paths the host role needs.
fn enrolled_server(temp: &Path, host_name: &str) -> (Service, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let server_root = temp.join("server");
    let host_root = temp.join("host");
    let server_config = temp.join("server.yaml");
    let host_config = temp.join("host.yaml");
    for (state, role, config) in [
        (&server_root, "server", &server_config),
        (&host_root, "host", &host_config),
    ] {
        let result = cli(state, &["init", role, "--output", config.to_str().unwrap()]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&server_config).unwrap()).unwrap();
    let ports = free_ports(4, false);
    for (i, name) in ["management", "inference", "bootstrap", "control"].iter().enumerate() {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], ports[i]));
        document["listeners"][name]["bind"] = addr.to_string().into();
        if matches!(*name, "bootstrap" | "control") {
            document["enrollment"][format!("{name}_address")] = format!("https://{addr}").into();
        }
    }
    fs::write(&server_config, serde_json::to_vec(&document).unwrap()).unwrap();
    let server = Service::start(&server_root, "server", &server_config);
    hosts(&server_root, &server_config, false, 0);
    let invitation = temp.join(format!("{host_name}.join"));
    let invited = cli(
        &server_root,
        &["invite", "host", "--name", host_name, "--config", server_config.to_str().unwrap(),
          "--output", invitation.to_str().unwrap()],
    );
    assert!(invited.status.success(), "{}", String::from_utf8_lossy(&invited.stderr));
    let joined = cli(
        &host_root,
        &["join", "host", "--join-file", invitation.to_str().unwrap(), "--config", host_config.to_str().unwrap()],
    );
    assert!(joined.status.success(), "{}", String::from_utf8_lossy(&joined.stderr));
    (server, server_root, server_config, host_root, host_config)
}

fn revoke(root: &Path, config: &Path, host: &str, request_id: Option<&str>) -> std::process::Output {
    let mut args = vec!["revoke", "host", host, "--config", config.to_str().unwrap(), "--output", "json"];
    if let Some(id) = request_id {
        args.extend(["--request-id", id]);
    }
    cli(root, &args)
}

// T06 (SPEC §§4.1, 13.3, 14; F3 revocation gate): `mllm revoke host` through
// the real binaries and mutual TLS. The live session closes, the host shows
// `revoked` and offline, its reconnects (including a restarted host role with
// the same identity) are refused, a replay with the same request identity is an
// idempotent no-op, one request identity cannot name another host, and an
// unknown host is `not_found`. CPU-only; the live row (M45) is separate.
#[test]
fn revoke_host_closes_the_session_and_keeps_the_host_out() {
    let temp = root();
    let (mut server, server_root, server_config, host_root, host_config) =
        enrolled_server(temp.path(), "revoked-spark");
    let mut host = Service::start(&host_root, "host", &host_config);
    let snapshot = hosts(&server_root, &server_config, true, 1);
    let id = snapshot["hosts"][0]["host_id"].as_str().unwrap().to_owned();
    assert_eq!(snapshot["hosts"][0]["revoked"], false);

    let request = ulid::Ulid::new().to_string();
    let out = revoke(&server_root, &server_config, "revoked-spark", Some(&request));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let revoked: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(revoked["host_id"], id.as_str());
    assert_eq!(revoked["name"], "revoked-spark");
    assert_eq!(revoked["revoked"], true);
    assert_eq!(revoked["newly_revoked"], true);
    assert_eq!(revoked["engines"], "retained");
    assert_eq!(revoked["request_id"], request.as_str());

    // The live session closes without the host role stopping.
    let listed = hosts(&server_root, &server_config, false, 1);
    assert_eq!(listed["hosts"][0]["revoked"], true);
    assert_eq!(listed["hosts"][0]["eligible"], false);
    // It keeps retrying with its old certificate and is refused every time,
    // and a restarted host role with the same identity fares no better.
    host.stop();
    host = Service::start(&host_root, "host", &host_config);
    for _ in 0..30 {
        let listed = hosts(&server_root, &server_config, false, 1);
        assert_eq!(listed["hosts"][0]["revoked"], true);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // SPEC §6.4: the same request identity replays; the host was already
    // revoked, so nothing changes.
    let out = revoke(&server_root, &server_config, &id, Some(&request));
    assert!(!out.status.success(), "one request identity named two spellings of the host");
    let out = revoke(&server_root, &server_config, "revoked-spark", Some(&request));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let replay: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(replay["newly_revoked"], false);
    let out = revoke(&server_root, &server_config, &id, None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let by_id: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(by_id["host_id"], id.as_str());
    assert_eq!(by_id["newly_revoked"], false);
    // SPEC §14: an unknown host is a typed `not_found`.
    let out = revoke(&server_root, &server_config, "no-such-host", None);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not_found"), "{stderr}");
    // Still out after a server restart: revocation is durable.
    server.stop();
    server = Service::start(&server_root, "server", &server_config);
    for _ in 0..20 {
        let listed = hosts(&server_root, &server_config, false, 1);
        assert_eq!(listed["hosts"][0]["revoked"], true);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    host.stop();
    server.stop();
}

// Test hygiene for the role-driving suites (support::process). T33: a role and
// anything it forked into its process group are gone when the test's guard
// drops, even though the grandchild was never known to the test.
#[test]
fn a_guarded_role_takes_its_process_group_with_it() {
    use std::io::BufRead as _;
    let mut role = Guarded::spawn(
        Command::new("sh")
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(std::process::Stdio::piped()),
    );
    let mut line = String::new();
    std::io::BufReader::new(role.child().stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let grandchild: i32 = line.trim().parse().unwrap();
    assert!(Path::new(&format!("/proc/{grandchild}")).exists());
    drop(role);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::fs::read_to_string(format!("/proc/{grandchild}/stat"))
        .is_ok_and(|stat| !stat.contains(") Z "))
    {
        assert!(std::time::Instant::now() < deadline, "the group outlived its guard");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

// Test hygiene: a start that must refuse but keeps running fails its test
// within the bound instead of hanging the suite, and is killed.
#[test]
fn a_refusal_that_never_exits_fails_within_its_bound() {
    let hung = std::panic::catch_unwind(|| {
        output_within(
            Command::new("sleep").arg("60"),
            std::time::Duration::from_millis(300),
        )
    });
    assert!(hung.is_err());
}

// Test hygiene: ports handed out for role listeners are distinct, free when
// chosen, and below the kernel's ephemeral range, where no outbound
// connection of a parallel test can take one before the role binds it.
#[test]
fn test_ports_avoid_the_ephemeral_range_and_never_repeat() {
    let low: u16 = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..50 {
        for port in free_ports(3, false) {
            assert!(port < low, "{port} is in the ephemeral range from {low}");
            assert!(seen.insert(port), "{port} handed out twice");
        }
    }
    let run = free_ports(4, true);
    assert!(run.windows(2).all(|pair| pair[1] == pair[0] + 1), "{run:?}");
}
