use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::*;
use crate::process_absence::{exited_unreaped, presence, verify_gone, GoneProof, Presence};

const FIXTURE: &str = "CAPYCTL_SUBREAPER_FIXTURE";
const PASSED: &str = "subreaper fixture passed";

fn stat_line(state: &str, threads: u64) -> String {
    // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt
    // cmajflt utime stime cutime cstime priority nice num_threads itrealvalue
    // starttime ...
    format!("42 (a (b) c) {state} 7 9 9 0 -1 0 0 0 0 0 0 0 0 0 20 0 {threads} 0 12345 0 0")
}

// T12: the fields are read after the last ')', whatever the command name holds.
#[test]
fn stat_fields_are_read_after_the_command_name() {
    let stat = parse_stat(&stat_line("S", 4)).unwrap();
    assert_eq!(
        stat,
        ProcStat {
            state: 'S',
            ppid: 7,
            pgrp: 9,
            threads: 4,
            start_ticks: 12345,
        }
    );
    assert!(!stat.exited());
    assert!(parse_stat("42 (x) S 7").is_none());
    assert!(parse_stat(&stat_line("SZ", 1)).is_none());
}

// T12: a zombie with no other threads has exited; a zombie thread-group leader
// whose threads still run is a live process.
#[test]
fn only_a_zombie_without_live_threads_has_exited() {
    assert!(parse_stat(&stat_line("Z", 1)).unwrap().exited());
    assert!(parse_stat(&stat_line("X", 1)).unwrap().exited());
    assert!(!parse_stat(&stat_line("Z", 3)).unwrap().exited());
}

// Reaping is armed only by a role's start: an unarmed process (this test
// process) never waits for a child someone else spawned.
#[test]
fn nothing_is_reaped_before_start() {
    if std::env::var_os(FIXTURE).is_some() {
        return;
    }
    assert!(!active());
    assert_eq!(reap_orphans(), 0);
    assert!(!reap_exited(std::process::id(), 1));
}

// T12 T33 (SPEC §13.2): run in a child process, because a subreaper changes
// where every orphan of this test binary goes, and its reaper must not race the
// other tests' own children.
#[test]
fn an_inherited_zombie_is_reaped_and_one_that_cannot_be_is_reported() {
    if std::env::var_os(FIXTURE).is_some() {
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "subreaper::tests::subreaper_fixture",
            "--nocapture",
        ])
        .env(FIXTURE, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains(PASSED),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A shell in a process group of its own, as an engine is: whatever it leaves
/// behind is not in this process's group, so the reaper may wait for it.
fn shell(script: &str) -> std::process::Child {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    spawn_direct(&mut command).unwrap()
}

fn first_pid(child: &mut std::process::Child) -> u32 {
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    line.trim().parse().unwrap()
}

/// Kill a child started in the background of a [`shell`]. The fixture's
/// children exit only this way, once their shell can no longer reap them: a
/// shell reaps a background child that has already exited when it next
/// finishes a command, built-ins included (`echo $!`), so a child that exits
/// on its own can be gone before it is handed here or seen as a zombie.
fn kill(pid: u32) {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn subreaper_fixture() {
    if std::env::var_os(FIXTURE).is_none() {
        return;
    }
    assert_eq!(start(), Ok(Supervision::Subreaper));
    assert!(nix::sys::prctl::get_child_subreaper().unwrap());
    let me = std::process::id();

    // An engine that exits and leaves a child behind: the child is handed to
    // this subreaper, exits in turn, and is reaped here, so it reads gone.
    let mut engine = shell("/bin/sleep 30 & echo $!");
    let orphan = first_pid(&mut engine);
    let identity = crate::exec::process_identity(orphan, "helper-0").unwrap();
    assert!(
        engine.wait().unwrap().success(),
        "the spawner still reaps its own child"
    );
    assert_eq!(
        proc_stat(orphan).unwrap().ppid,
        me,
        "the orphan is handed here"
    );
    kill(orphan);
    until("the orphan is reaped", || {
        presence(&identity) == Presence::Gone
    });
    assert!(!std::path::Path::new(&format!("/proc/{orphan}")).exists());
    assert_eq!(
        verify_gone(std::slice::from_ref(&identity)),
        GoneProof::AllGone
    );

    // A zombie whose parent is another live process that never waits: it
    // cannot be reaped here, and it is reported, neither alive nor gone. The
    // child exits only once its parent runs `sleep`, which never waits.
    let mut parent = shell("/bin/sleep 30 & echo $!; exec /bin/sleep 30");
    let zombie = first_pid(&mut parent);
    let identity = crate::exec::process_identity(zombie, "helper-0").unwrap();
    until("the parent no longer runs the shell", || {
        std::fs::read_to_string(format!("/proc/{}/comm", parent.id()))
            .is_ok_and(|comm| comm.trim_end() == "sleep")
    });
    kill(zombie);
    until("the child exits", || {
        proc_stat(zombie).is_some_and(|stat| stat.exited())
    });
    assert_eq!(
        reap_orphans(),
        0,
        "another process's zombie is not ours to reap"
    );
    assert_eq!(presence(&identity), Presence::Unknown);
    assert!(exited_unreaped(&identity));
    assert_eq!(
        verify_gone(std::slice::from_ref(&identity)),
        GoneProof::Unreaped
    );

    // Once its parent is gone the zombie is handed here, and reaped.
    parent.kill().unwrap();
    parent.wait().unwrap();
    until("the handed-over zombie is reaped", || {
        presence(&identity) == Presence::Gone
    });
    assert!(!exited_unreaped(&identity));
    println!("{PASSED}");
}
