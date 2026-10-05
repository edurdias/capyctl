//! Process and port hygiene for the tests that drive the real `capyctl` binary.
//!
//! Three failure modes made these suites flaky when run with the default
//! parallel test threads, and each has one answer here:
//!
//! - **Leaked roles.** A role spawned and then waited on for a ready line
//!   leaked when the wait panicked, because the guard that kills it was only
//!   built after the wait. [`Guarded::spawn`] returns the guard first, so every
//!   later panic unwinds through its `Drop`. A test process killed outright
//!   (a harness timeout, SIGKILL) runs no `Drop` at all, so each role is also
//!   started with `PR_SET_PDEATHSIG` and dies with the thread that spawned it.
//! - **Leaked descendants.** Each role runs in its own process group, and the
//!   guard kills the whole group, so a helper the role forked into its group
//!   goes with it. Engines start their own groups and are cleaned up by the
//!   test fixture that knows their pids.
//! - **Port races.** A port taken from `bind("127.0.0.1:0")` and released comes
//!   from the kernel's ephemeral range, which is exactly where every outbound
//!   connection in a parallel test (a reqwest client, a gRPC channel, a CLI
//!   child) picks its local port. [`free_port`] and [`free_ports`] choose from
//!   below that range instead, through `capyctl_testkit::ports`: each test
//!   binary claims blocks of ports under a file lock, so no two binaries running
//!   at once hand out the same port, and every port of a set is checked while
//!   holding all of them, so a set is internally distinct and free when chosen.
//!
//! None of this is qualification of any engine recipe (SPEC §18).

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

/// A child process that is killed, with its whole process group, when this
/// guard drops. Construct it only through [`Guarded::spawn`].
pub struct Guarded {
    child: Child,
}

impl Guarded {
    /// Spawn `command` in its own process group, with `PR_SET_PDEATHSIG`
    /// set to SIGKILL, and return the guard before anything else can fail.
    ///
    /// The death signal fires when the *thread* that spawned the child exits.
    /// Tests spawn roles from the test's own thread (a `#[tokio::test]` body
    /// runs on it through `block_on`), which lives until the test ends; never
    /// spawn a role from a short-lived helper thread.
    pub fn spawn(command: &mut Command) -> Self {
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: the closure runs in the forked child before exec and calls
        // only async-signal-safe functions (prctl, getppid).
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have died between fork and prctl.
                if libc::getppid() != parent {
                    return Err(std::io::Error::other("the test process is gone"));
                }
                Ok(())
            });
        }
        command.process_group(0);
        Self {
            child: command.spawn().expect("the capyctl binary starts"),
        }
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Send `signal` to the role process itself (not its group).
    pub fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(self.child.id() as i32, signal);
        }
    }

    /// Wait for the role to exit on its own within `within`; panics (and the
    /// guard then kills it) when it does not.
    pub fn exit_within(&mut self, within: Duration, what: &str) -> ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{what} did not exit within {within:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        // The group id is the child's pid (process_group(0)). A pid cannot be
        // reused while a process group of that id still has members, so this
        // reaches only the role's own group.
        unsafe {
            libc::killpg(self.child.id() as i32, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run `command` to completion under a guard, failing the test if it is still
/// running after `within`. For negative role starts, which must refuse and
/// exit: if the refusal regressed, the role would serve forever and a plain
/// `output()` would hang the suite instead of failing it.
pub fn output_within(command: &mut Command, within: Duration) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut guarded = Guarded::spawn(command);
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        guarded
            .child()
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        guarded
            .child()
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + within;
    let status = loop {
        if let Some(status) = guarded.child().try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            // Kill first so the pipes close and the readers finish.
            drop(guarded);
            let said = String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned();
            panic!("the command was still running after {within:?}; stderr: {said}");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }
}

/// `count` loopback ports, `consecutive` or not, each bindable at the moment
/// the whole set is chosen and none handed out before by any test binary
/// running now (`capyctl_testkit::ports`).
pub fn free_ports(count: usize, consecutive: bool) -> Vec<u16> {
    capyctl_testkit::ports::free_ports(count, consecutive)
}

/// One loopback port (see [`free_ports`]).
pub fn free_port() -> u16 {
    free_ports(1, false)[0]
}

/// A loopback address on a [`free_port`].
pub fn free_address() -> String {
    format!("127.0.0.1:{}", free_port())
}
