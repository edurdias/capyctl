//! Real exec launcher: process groups, signal escalation, start-identity
//! handle verification (T12 mechanics; F1 design §4).

use std::time::Duration;

use capyctl_adapters::traits::{Launcher, OwnedHandle, RenderedCommand};
use capyctl_launchers::ExecLauncher;

fn sleep_cmd(secs: u32) -> RenderedCommand {
    RenderedCommand {
        argv: vec!["sleep".into(), secs.to_string()],
        env: Default::default(),
    }
}

fn respawn_cmd() -> RenderedCommand {
    RenderedCommand {
        argv: vec!["true".into()],
        env: Default::default(),
    }
}

#[test]
fn spawn_creates_live_handle_and_terminate_stops_group() {
    let l = ExecLauncher::new();
    let h = l.spawn(&sleep_cmd(30)).unwrap();
    assert!(matches!(
        l.verify_handle(&h),
        capyctl_adapters::traits::HandleStatus::Valid
    ));
    let rep = l.terminate(&h, Duration::from_secs(1)).unwrap();
    assert!(matches!(
        l.verify_handle(&h),
        capyctl_adapters::traits::HandleStatus::Gone
            | capyctl_adapters::traits::HandleStatus::StaleReused
    ));
    let _ = rep;
}

#[test]
fn terminate_reports_signal_when_grace_expires() {
    // `sleep` ignores nothing special, but SIGTERM default-kills it; use a
    // process that ignores TERM to exercise escalation: `sh -c 'trap "" TERM; sleep 5'`.
    let l = ExecLauncher::new();
    let dir = tempfile::tempdir().unwrap();
    let trapped = dir.path().join("trapped");
    let cmd = RenderedCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "trap \"\" TERM; : > '{}'; while :; do sleep 1; done",
                trapped.display()
            ),
        ],
        env: Default::default(),
    };
    let h = l.spawn(&cmd).unwrap();
    // Signal only once the shell says its TERM trap is installed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !trapped.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(trapped.exists(), "the shell installed its TERM trap");
    let rep = l.terminate(&h, Duration::from_millis(300)).unwrap();
    assert!(rep.killed, "escalated to SIGKILL after grace expiry");
    assert_eq!(rep.signal, Some(9), "SIGKILL");
}

#[test]
fn terminate_graceful_reports_exit() {
    let l = ExecLauncher::new();
    let h = l.spawn(&sleep_cmd(30)).unwrap();
    let rep = l.terminate(&h, Duration::from_secs(5)).unwrap();
    assert!(!rep.killed);
    assert!(rep.exit_code.is_some() || rep.signal.is_some());
}

#[test]
fn respawned_process_has_new_start_identity() {
    let l = ExecLauncher::new();
    let h1 = l.spawn(&respawn_cmd()).unwrap();
    let _ = l.terminate(&h1, Duration::from_secs(1)).unwrap();
    let h2 = l.spawn(&respawn_cmd()).unwrap();
    assert_ne!(h1.start_identity, h2.start_identity);
    let _ = l.terminate(&h2, Duration::from_millis(500)).unwrap();
}

#[test]
fn stale_handle_with_wrong_starttime_is_rejected() {
    let l = ExecLauncher::new();
    // PID 1 (init) exists with a different start identity than ours:
    let stale = OwnedHandle {
        pid: 1,
        start_identity: 0xdeadbeef,
    };
    assert!(matches!(
        l.verify_handle(&stale),
        capyctl_adapters::traits::HandleStatus::StaleReused
    ));
    let h = l.spawn(&sleep_cmd(30)).unwrap();
    assert!(matches!(
        l.verify_handle(&h),
        capyctl_adapters::traits::HandleStatus::Valid
    ));
    let _ = l.terminate(&h, Duration::from_millis(300)).unwrap();
}

#[test]
fn never_spawned_pid_is_gone() {
    let l = ExecLauncher::new();
    // Find an almost-certainly-unbound pid: kernel pid_max - 1.
    let gone = OwnedHandle {
        pid: 4000000,
        start_identity: 0,
    };
    assert!(matches!(
        l.verify_handle(&gone),
        capyctl_adapters::traits::HandleStatus::Gone
    ));
}
