//! Custom engine adapter module (ADR 0031): an engine CapyCTL has no
//! integration for, started from a host-approved manifest. The command and
//! closed environment rendering, the shared launch builder, the bounded reads
//! of the leased port, the health path and the idle signal, the loopback
//! check of the owned listeners, and the restart-only adapter. The wait
//! before a stop signal is the shared [`crate::tensorfold::wait_idle`] over
//! this adapter's idle signal.
pub mod adapter;
pub mod args;
mod frozen;
pub mod http;
mod initialize;
pub mod listeners;

pub use adapter::CustomAdapter;
pub use args::{engine_environment, render_command, CustomArgsError, PlanInputCustom};
pub use frozen::{plan_from_effective, CustomPlanError};
pub use http::{idle_reading, READ_TIMEOUT as HEALTH_READ_TIMEOUT};
pub use listeners::{check_loopback, ListenerCheck};
