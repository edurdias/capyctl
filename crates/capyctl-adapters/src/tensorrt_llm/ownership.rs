//! ADR 0038 §9: the two ownership checks every TensorRT-LLM launch passes at
//! readiness and whenever a host re-proves it: no descendant of the engine
//! outside the launch's process group (`engine_process_escaped`), and no
//! listener of an owned process off loopback (`engine_listener_exposed`,
//! ADR 0031 §7's check, run on every launch rather than only on several
//! GPUs). Either failure stops the launch; an escaped process must be proven
//! gone beside the group, since a group stop cannot reach it.
use std::path::Path;

use capyctl_domain::completion::{Presence, ProcessIdentity};

use crate::custom::listeners::{check_engine_launch, engine_listener_exposed, ListenerCheck};
use crate::process_tree::{escaped_descendants, EscapedProcess, ENGINE_PROCESS_ESCAPED};

/// Why a launch's ownership was not proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipFailure {
    /// Descendants of the engine outside the launch's process group, in the
    /// order found (never empty).
    Escaped(Vec<EscapedProcess>),
    /// An owned process listens off loopback.
    Exposed(crate::custom::listeners::OwnedListener),
    /// Nothing could be proven either way.
    Unreadable(String),
}

impl OwnershipFailure {
    /// The closed launch failure, or `None` when nothing was proven.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Escaped(_) => Some(ENGINE_PROCESS_ESCAPED),
            Self::Exposed(_) => Some(capyctl_domain::local_tp::ENGINE_LISTENER_EXPOSED),
            Self::Unreadable(_) => None,
        }
    }

    /// The identities a stop of the launch must prove gone: the recorded
    /// ones and, for an escape, every escaped process (each named
    /// `escaped-N`, which a verified stop signals one by one).
    pub fn stop_set(&self, identities: &[ProcessIdentity]) -> Vec<ProcessIdentity> {
        let mut set = identities.to_vec();
        if let Self::Escaped(escaped) = self {
            set.extend(escaped.iter().map(|process| process.identity.clone()));
        }
        set
    }
}

impl std::fmt::Display for OwnershipFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Escaped(escaped) => {
                let named: Vec<String> = escaped.iter().map(ToString::to_string).collect();
                f.write_str(&named.join("; "))
            }
            Self::Exposed(listener) => f.write_str(&engine_listener_exposed(listener)),
            Self::Unreadable(reason) => {
                write!(f, "the launch's ownership could not be proven: {reason}")
            }
        }
    }
}

/// ADR 0038 §9: both checks over a launch's recorded processes
/// `identities` (the `api` leader among them), read under `proc_root`
/// (`/proc` on a host). The descendants first, then the listeners; a helper
/// that exits during the listener read is left out and the rest read again
/// ([`check_engine_launch`]).
pub fn check_ownership(
    proc_root: &Path,
    identities: &[ProcessIdentity],
    presence: &dyn Fn(&ProcessIdentity) -> Presence,
) -> Result<(), OwnershipFailure> {
    let leader = identities
        .iter()
        .find(|identity| identity.role == "api")
        .ok_or_else(|| OwnershipFailure::Unreadable("no api process is recorded".into()))?;
    let escaped = escaped_descendants(proc_root, leader)
        .map_err(|unreadable| OwnershipFailure::Unreadable(unreadable.to_string()))?;
    if !escaped.is_empty() {
        return Err(OwnershipFailure::Escaped(escaped));
    }
    match check_engine_launch(proc_root, identities, presence) {
        Ok(()) => Ok(()),
        Err(ListenerCheck::Exposed(listener)) => Err(OwnershipFailure::Exposed(listener)),
        Err(other) => Err(OwnershipFailure::Unreadable(other.to_string())),
    }
}
