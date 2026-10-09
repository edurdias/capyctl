//! SPEC §13.3 / T21: the `capyctl` binary is the redacting writer its
//! launchers start (`/proc/self/exe __engine-log-relay`). This runs the built
//! binary in that mode exactly as a launcher does: engine output on standard
//! input, the launch's credentials on descriptor 3, the log named by
//! `CAPYCTL_ENGINE_LOG`.
mod support;

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;

use capyctl_adapters::engine_log::LogRedactor;
use capyctl_launchers::engine_log_relay::{LOG_VARIABLE, RELAY_ARG};

const KEY: &str = "owned-admin-credential-0001";

// T21
#[test]
fn the_binary_relays_engine_output_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("launch.log");
    let mut redactor = LogRedactor::new();
    redactor.own(KEY);
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (secrets_read, secrets_write) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let read_fd = secrets_read.as_raw_fd();
    let write_fd = secrets_write.as_raw_fd();
    let mut command = support::capyctl();
    command
        .arg(RELAY_ARG)
        .env_clear()
        .env(LOG_VARIABLE, &log)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            libc::close(write_fd);
            if libc::dup2(read_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(secrets_read);
    std::fs::File::from(secrets_write)
        .write_all(&redactor.encode())
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "admin {KEY}\nAuthorization: Bearer abc.def\nfetch https://u:p@h.example/x?sig=1\nplain\n"
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(child.wait().unwrap().success());
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        text,
        "admin <redacted>\nAuthorization: <redacted>\nfetch https://<redacted>@h.example/x?<redacted>\nplain\n"
    );
}
