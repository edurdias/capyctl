//! What a role sets up about its own process before it serves, so it behaves
//! the same on a host, under a service manager and inside a container:
//!
//! - SPEC §13.2 / T12: it reaps the processes it inherits. It becomes a child
//!   subreaper, so the orphans of its engines come to it, and as PID 1 of a
//!   container without an init it reaps every orphan there; a zombie it reaped
//!   reads gone and cleanup can complete.
//! - SPEC §7.2 / T26: it says which source bounds the memory it observes when
//!   that is not the host's `/proc/meminfo` alone (a cgroup v2 limit, or cgroup
//!   limits it cannot read).

use capyctl_agent::memory::MemorySource;
use capyctl_domain::role_log::{notice, Level};
use capyctl_launchers::subreaper::Supervision;

/// Called once at role start, before any engine is launched.
pub fn start() {
    match capyctl_launchers::subreaper::start() {
        Ok(Supervision::Init) => notice(
            Level::Notice,
            "Running as PID 1: this role reaps every orphaned process in its PID namespace.",
        ),
        Ok(Supervision::Subreaper) => {}
        Err(error) => notice(
            Level::Warning,
            &format!(
                "This role cannot reap the processes it inherits ({error}); an engine's exited children may block cleanup unless an init reaps them."
            ),
        ),
    }
    // A failed reading is reported where the role reads memory to publish it.
    let Ok(reading) = capyctl_agent::memory::read_host_memory() else {
        return;
    };
    match reading.source {
        MemorySource::Meminfo => {}
        MemorySource::CgroupV2 { .. } => notice(
            Level::Notice,
            &format!(
                "{} Capacity {} MiB, available {} MiB.",
                reading.source.describe(),
                reading.memory.capacity_bytes >> 20,
                reading.memory.available_bytes >> 20
            ),
        ),
        MemorySource::CgroupV1Unread | MemorySource::CgroupV2Unreadable => {
            notice(Level::Warning, &reading.source.describe())
        }
    }
}
