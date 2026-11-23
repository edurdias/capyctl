//! ADR 0040 §1: the management listener off the box with one explicit
//! setting (`--management-remote`, `CAPYCTL_MANAGEMENT_REMOTE`), and a client
//! on the box still reaching the role through the address it recorded.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod support;
use support::process::{free_port, Guarded};

use capyctl_cli::grammar::parse_invocation;

/// T03 / ADR 0040 §1: grammar defers authorization until YAML and --set are read.
#[test]
fn management_listen_off_loopback_needs_management_remote() {
    for role in ["standalone", "server"] {
        let i = parse_invocation([
            "capyctl",
            "start",
            role,
            "--management-listen",
            "0.0.0.0:7443",
            "--management-remote",
        ])
        .unwrap();
        assert_eq!(i.management_listen, Some("0.0.0.0:7443".parse().unwrap()));
        assert!(i.management_remote);
        let deferred = parse_invocation([
            "capyctl",
            "start",
            role,
            "--management-listen",
            "0.0.0.0:7443",
        ])
        .unwrap();
        assert!(!deferred.management_remote);
        let path = if role == "standalone" {
            "server.listeners.management.remote=true"
        } else {
            "listeners.management.remote=true"
        };
        let generic = parse_invocation([
            "capyctl",
            "start",
            role,
            "--management-listen",
            "0.0.0.0:7443",
            "--set",
            path,
        ])
        .unwrap();
        assert_eq!(generic.management_listen, deferred.management_listen);
    }
    // A host serves no management API.
    assert!(parse_invocation(["capyctl", "start", "host", "--management-remote"]).is_err());
}

/// ADR 0040 §1: the role records the address it binds; a client on the box
/// reads a recorded off-box address as it is and an unspecified one through
/// loopback on the same port.
#[test]
fn the_recorded_address_is_read_as_a_client_reaches_it() {
    let state = tempfile::tempdir().unwrap();
    let off_box: SocketAddr = "100.101.102.103:7443".parse().unwrap();
    capyctl_cli::roles::record_management_address(state.path(), off_box);
    assert_eq!(
        capyctl_cli::roles::recorded_management_address(state.path()),
        Some(off_box)
    );
    capyctl_cli::roles::record_management_address(state.path(), "0.0.0.0:7443".parse().unwrap());
    assert_eq!(
        capyctl_cli::roles::recorded_management_address(state.path()),
        Some("127.0.0.1:7443".parse().unwrap())
    );
    capyctl_cli::roles::record_management_address(state.path(), "[::]:7443".parse().unwrap());
    assert_eq!(
        capyctl_cli::roles::recorded_management_address(state.path()),
        Some("[::1]:7443".parse().unwrap())
    );
}

/// A standalone role under an owner-only state root of its own.
struct Installation {
    root: tempfile::TempDir,
}

impl Installation {
    fn new() -> Self {
        // The controller lock refuses a state path under a group- or
        // other-writable ancestor, so the root lives under HOME, owner-only.
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = support::capyctl();
        command
            .env("CAPYCTL_STATE_DIR", self.root.path().join("state"))
            .env(
                "CAPYCTL_INFERENCE_ADDR",
                format!("127.0.0.1:{}", free_port()),
            );
        command
    }

    /// Start `start standalone` with `args` and `env`, wait for its ready
    /// line, and run `list deployments` as a client that names no address.
    fn client_reaches(&self, args: &[&str], env: &[(&str, &str)]) {
        let mut command = self.command();
        command.args(["start", "standalone"]).args(args);
        for (name, value) in env {
            command.env(name, value);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut role = Guarded::spawn(&mut command);
        let stdout = role.child().stdout.take().unwrap();
        let (lines, received) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match received.recv_timeout(left) {
                Ok(line) if line.contains("\"role\":\"standalone\"") => break,
                Ok(_) => {}
                Err(error) => panic!("the role never said it was ready: {error}"),
            }
        }
        let listed = self
            .command()
            .args(["list", "deployments", "--json"])
            .output()
            .unwrap();
        assert!(
            listed.status.success(),
            "{}",
            String::from_utf8_lossy(&listed.stderr)
        );
        role.signal(libc::SIGTERM);
        let status = role.exit_within(Duration::from_secs(30), "the standalone role");
        assert!(status.success(), "{status:?}");
    }
}

/// ADR 0040 §1: with `CAPYCTL_MANAGEMENT_REMOTE=1` and no flag the role binds
/// `0.0.0.0`, as with `--management-remote`, and a CLI command on the box
/// reaches it through loopback.
#[test]
fn an_unspecified_bind_is_reached_through_loopback() {
    let installation = Installation::new();
    let bind = format!("0.0.0.0:{}", free_port());
    installation.client_reaches(
        &["--management-listen", &bind],
        &[("CAPYCTL_MANAGEMENT_REMOTE", "1")],
    );
}

/// ADR 0040 §1: without `remote` the same start is refused before the role
/// binds anything.
#[test]
fn an_unspecified_bind_without_remote_is_refused() {
    let installation = Installation::new();
    let bind = format!("0.0.0.0:{}", free_port());
    let out = installation
        .command()
        .args(["start", "standalone", "--management-listen", &bind])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("must be a loopback address"), "{said}");
}

/// This box's own non-loopback address, if it has one (no packet is sent).
fn non_loopback_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// ADR 0040 §1: a role bound with `--management-remote` at one of the box's
/// own off-loopback addresses (a Tailscale address in practice) is reached by
/// a CLI command on the box at that address, as recorded.
#[test]
fn a_recorded_off_box_address_is_reached_as_it_is() {
    let Some(ip) = non_loopback_address() else {
        eprintln!("skipped: this machine has no non-loopback address");
        return;
    };
    let installation = Installation::new();
    let bind = SocketAddr::new(ip, free_port()).to_string();
    installation.client_reaches(&["--management-listen", &bind, "--management-remote"], &[]);
}
