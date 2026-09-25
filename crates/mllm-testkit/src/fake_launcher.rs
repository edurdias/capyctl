//! A deterministic fake [`mllm_adapters::traits::Launcher`] with PID-reuse injection.
//!
//! `with_pid_reuse()` makes the next spawn after a terminate reuse the same
//! PID with a new start identity — exactly the hazard the boot-unique
//! `start_identity` field exists to detect.

use mllm_adapters::traits::*;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug)]
struct LauncherState {
    next_pid: u32,
    next_identity: u128,
    /// pid -> start identity of the currently live process, if any.
    live: BTreeMap<u32, u128>,
    /// The first PID handed out; reused on respawn when `pid_reuse` is set,
    /// even if the process is no longer live.
    base_pid: Option<u32>,
}

/// Deterministic in-process launcher.
#[derive(Debug)]
pub struct FakeLauncher {
    pid_reuse: bool,
    state: Mutex<LauncherState>,
}

impl FakeLauncher {
    pub fn new() -> Self {
        Self {
            pid_reuse: false,
            state: Mutex::new(LauncherState {
                next_pid: 4242,
                next_identity: 1,
                live: BTreeMap::new(),
                base_pid: None,
            }),
        }
    }

    /// Reuse the first PID on respawn, with a fresh start identity.
    pub fn with_pid_reuse(mut self) -> Self {
        self.pid_reuse = true;
        self
    }
}

impl Default for FakeLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl Launcher for FakeLauncher {
    fn spawn(&self, _cmd: &RenderedCommand) -> Result<OwnedHandle, LauncherError> {
        let mut st = self.state.lock().unwrap();
        let pid = if self.pid_reuse {
            match st.base_pid {
                Some(pid) => pid,
                None => {
                    let pid = st.next_pid;
                    st.next_pid += 1;
                    st.base_pid = Some(pid);
                    pid
                }
            }
        } else {
            let pid = st.next_pid;
            st.next_pid += 1;
            pid
        };
        let identity = st.next_identity;
        st.next_identity += 1;
        st.live.insert(pid, identity);
        Ok(OwnedHandle {
            pid,
            start_identity: identity,
        })
    }

    fn terminate(&self, h: &OwnedHandle, _grace: Duration) -> Result<ExitReport, LauncherError> {
        let mut st = self.state.lock().unwrap();
        match st.live.get(&h.pid) {
            Some(id) if *id == h.start_identity => {
                st.live.remove(&h.pid);
                Ok(ExitReport {
                    pid: h.pid,
                    exit_code: Some(0),
                    signal: None,
                    killed: true,
                })
            }
            _ => Err(LauncherError::TerminateFailed(format!(
                "pid {} is not owned by this handle",
                h.pid
            ))),
        }
    }

    fn verify_handle(&self, h: &OwnedHandle) -> HandleStatus {
        let st = self.state.lock().unwrap();
        match st.live.get(&h.pid) {
            Some(id) if *id == h.start_identity => HandleStatus::Valid,
            Some(_) => HandleStatus::StaleReused,
            None => HandleStatus::Gone,
        }
    }
}
