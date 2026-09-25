//! ADR 0018 §3: the host's runtime profiles as the server accepted them, the
//! one it is being asked to accept, and the one before. Launch plans are
//! authorized against whichever of these their fingerprint names, so a
//! publication never strands a command already sent or being redelivered.
use crate::installation::{register_profile, InstallationMeasurer, InstallationRegistry};
use mllm_config::remote_roles::HostConfig;
use mllm_protocol::pb;
use std::sync::{Arc, RwLock};

/// ADR 0008: each declared profile measured (version, digest); unmeasurable
/// is never a refusal. Moved from `mllm-cli`'s host start.
pub fn profile_statuses(config: &HostConfig) -> Vec<pb::RuntimeProfileStatus> {
    let measurer = InstallationMeasurer::new();
    config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let mut status = pb::RuntimeProfileStatus {
                name: name.clone(),
                build_fingerprint: profile["build_fingerprint"]
                    .as_str()
                    .unwrap_or("unknown")
                    .into(),
                eligibility: "unknown".into(),
                reason: String::new(),
                ..Default::default()
            };
            register_profile(&measurer, profile, &mut status);
            status
        })
        .collect()
}

/// One document's runtime profiles: its configuration, the inventory that
/// describes it, the installations measured for it, and its fingerprint.
pub struct ProfileSet {
    pub config: HostConfig,
    pub inventory: pb::ReportInventory,
    pub installations: Arc<InstallationRegistry>,
    pub fingerprint: String,
}

impl ProfileSet {
    /// The fingerprint is the inventory's `policy_fingerprint`, the one plans
    /// name.
    pub fn new(config: HostConfig, inventory: pb::ReportInventory) -> Self {
        let installations = Arc::new(InstallationRegistry::from_inventory(&inventory));
        let fingerprint = inventory.policy_fingerprint.clone();
        Self {
            config,
            inventory,
            installations,
            fingerprint,
        }
    }

    /// ADR 0018 §3: the host measures its profiles again and describes the
    /// new document; domains, boot id and launch claims are unchanged.
    pub fn measure(config: HostConfig, base: &pb::ReportInventory) -> Self {
        let mut inventory = base.clone();
        inventory.profiles = profile_statuses(&config);
        inventory.approved_host_config_json = config.document.to_string();
        inventory.policy_fingerprint =
            mllm_config::remote_resources::policy_fingerprint(&config.document);
        Self::new(config, inventory)
    }
}

/// Another publication is already waiting for the server's verdict.
#[derive(Debug)]
pub struct Busy;

struct State {
    accepted: Arc<ProfileSet>,
    previous: Option<Arc<ProfileSet>>,
    pending: Option<(String, Arc<ProfileSet>)>,
}

/// ADR 0018 §3: the accepted set, at most one pending publication, and the
/// set accepted before the current one.
pub struct HostProfiles {
    state: RwLock<State>,
}

impl HostProfiles {
    pub fn new(set: ProfileSet) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(State {
                accepted: Arc::new(set),
                previous: None,
                pending: None,
            }),
        })
    }
    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    pub fn accepted(&self) -> Arc<ProfileSet> {
        self.read().accepted.clone()
    }
    pub fn pending(&self) -> Option<(String, Arc<ProfileSet>)> {
        self.read().pending.clone()
    }
    pub fn publishing(&self) -> bool {
        self.read().pending.is_some()
    }
    /// The accepted, pending or previous set `fingerprint` names, if any.
    pub fn for_fingerprint(&self, fingerprint: &str) -> Option<Arc<ProfileSet>> {
        let state = self.read();
        std::iter::once(&state.accepted)
            .chain(state.pending.as_ref().map(|(_, set)| set))
            .chain(state.previous.as_ref())
            .find(|set| set.fingerprint == fingerprint)
            .cloned()
    }
    /// ADR 0018 §3: one publication at a time.
    pub fn stage(&self, request_id: &str, set: ProfileSet) -> Result<Arc<ProfileSet>, Busy> {
        let mut state = self.write();
        if state.pending.is_some() {
            return Err(Busy);
        }
        let set = Arc::new(set);
        state.pending = Some((request_id.to_owned(), set.clone()));
        Ok(set)
    }
    /// The server's verdict on `request_id`: promote or drop. `false` when
    /// nothing was pending under that id.
    pub fn settle(&self, request_id: &str, accepted: bool) -> bool {
        let mut state = self.write();
        match state.pending.take() {
            Some((id, set)) if id == request_id => {
                if accepted {
                    let old = std::mem::replace(&mut state.accepted, set);
                    state.previous = Some(old);
                }
                true
            }
            other => {
                state.pending = other;
                false
            }
        }
    }
    /// Startup and unit tests: replace the accepted set outright.
    pub fn replace(&self, set: ProfileSet) {
        let mut state = self.write();
        state.accepted = Arc::new(set);
        state.previous = None;
        state.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(fingerprint: &str) -> ProfileSet {
        let dir = std::path::Path::new("/home/operator/host");
        let config = HostConfig::parse(&HostConfig::template(dir)).unwrap();
        let inventory = pb::ReportInventory {
            policy_fingerprint: fingerprint.into(),
            ..Default::default()
        };
        ProfileSet::new(config, inventory)
    }

    // T34 (ADR 0018 §3): a plan is authorized against the accepted, pending
    // or previous document, so an in-flight or redelivered command survives a
    // publication; older ones are forgotten.
    #[test]
    fn a_plan_is_authorized_against_accepted_pending_or_previous() {
        let profiles = HostProfiles::new(set("a"));
        profiles.stage("r1", set("b")).unwrap();
        assert!(profiles.publishing());
        for fp in ["a", "b"] {
            assert!(profiles.for_fingerprint(fp).is_some(), "{fp}");
        }
        assert!(profiles.settle("r1", true));
        assert_eq!(profiles.accepted().fingerprint, "b");
        assert!(profiles.for_fingerprint("a").is_some(), "previous kept");
        profiles.stage("r2", set("c")).unwrap();
        profiles.settle("r2", true);
        assert!(
            profiles.for_fingerprint("a").is_none(),
            "only one previous is kept"
        );
        assert!(profiles.for_fingerprint("b").is_some());
    }

    // T07: a refused publication drops only the pending set.
    #[test]
    fn a_refused_publication_drops_only_the_pending_set() {
        let profiles = HostProfiles::new(set("a"));
        profiles.stage("r1", set("b")).unwrap();
        assert!(profiles.settle("r1", false));
        assert_eq!(profiles.accepted().fingerprint, "a");
        assert!(!profiles.publishing());
        assert!(profiles.for_fingerprint("b").is_none());
        assert!(!profiles.settle("unknown", true));
    }

    // ADR 0018 §3: one publication at a time.
    #[test]
    fn one_publication_at_a_time() {
        let profiles = HostProfiles::new(set("a"));
        profiles.stage("r1", set("b")).unwrap();
        assert!(profiles.stage("r2", set("c")).is_err());
    }
}
