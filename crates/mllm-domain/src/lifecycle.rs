use std::sync::OnceLock;

/// The single source of truth for legal lifecycle transitions (spec 6.1).
static LEGAL: OnceLock<Vec<(LifecycleState, LifecycleState)>> = OnceLock::new();

/// Every legal `(from, to)` transition pair.
pub fn legal_transitions() -> &'static [(LifecycleState, LifecycleState)] {
    LEGAL.get_or_init(|| {
        use LifecycleState::*;
        vec![
            // happy path around the park cycle
            (Stopped, Starting),
            (Starting, Ready),
            (Ready, Draining),
            (Draining, Parking),
            (Parking, Parked),
            (Parked, Waking),
            (Waking, Ready),
            // drain-to-stop path
            (Draining, Stopping),
            (Stopping, Stopped),
            (Parked, Stopping),
            // any state may enter reconciliation
            (Stopped, Reconciling),
            (Starting, Reconciling),
            (Ready, Reconciling),
            (Draining, Reconciling),
            (Parking, Reconciling),
            (Parked, Reconciling),
            (Waking, Reconciling),
            (Stopping, Reconciling),
            (Failed, Reconciling),
            // reconciliation outcomes
            (Reconciling, Stopped),
            (Reconciling, Ready),
            (Reconciling, Failed),
        ]
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    Stopped,
    Starting,
    Ready,
    Draining,
    Parking,
    Parked,
    Waking,
    Stopping,
    Reconciling,
    Failed,
}

impl LifecycleState {
    pub const ALL: &'static [LifecycleState] = &[
        Self::Stopped,
        Self::Starting,
        Self::Ready,
        Self::Draining,
        Self::Parking,
        Self::Parked,
        Self::Waking,
        Self::Stopping,
        Self::Reconciling,
        Self::Failed,
    ];

    /// True iff the state's outcome is not yet decided.
    pub fn is_uncertain(&self) -> bool {
        matches!(self, Self::Reconciling)
    }

    pub fn can_transition_to(&self, to: Self) -> bool {
        legal_transitions().contains(&(*self, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LifecycleState::*;
    use proptest::prelude::*;

    #[test]
    fn legal_paths_from_spec_6_1() {
        assert!(Stopped.can_transition_to(Starting));
        assert!(Starting.can_transition_to(Ready));
        assert!(Ready.can_transition_to(Draining));
        assert!(Draining.can_transition_to(Parking));
        assert!(Parking.can_transition_to(Parked));
        assert!(Parked.can_transition_to(Waking));
        assert!(Waking.can_transition_to(Ready));
        assert!(Draining.can_transition_to(Stopping));
        assert!(Stopping.can_transition_to(Stopped));
        assert!(Parked.can_transition_to(Stopping));
        for s in [Stopped, Starting, Ready, Draining, Parking, Parked, Waking, Stopping, Failed] {
            assert!(s.can_transition_to(Reconciling), "{s:?} -> RECONCILING");
        }
        assert!(Reconciling.can_transition_to(Failed));
    }

    #[test]
    fn illegal_paths_rejected() {
        assert!(!Ready.can_transition_to(Parked)); // must drain first
        assert!(!Parked.can_transition_to(Starting)); // must wake
        assert!(!Stopped.can_transition_to(Ready)); // no cold jump
        assert!(!Failed.can_transition_to(Ready)); // recovery first
    }

    #[test]
    fn reconciling_is_the_only_uncertain_state() {
        for s in [Stopped, Starting, Ready, Draining, Parking, Parked, Waking, Stopping, Failed] {
            assert!(!s.is_uncertain());
        }
        assert!(Reconciling.is_uncertain());
    }

    proptest! {
        #[test]
        fn every_legal_transition_pair_is_symmetric_in_table(
            from in prop::sample::select(LifecycleState::ALL.to_vec()),
            to in prop::sample::select(LifecycleState::ALL.to_vec()),
        ) {
            prop_assert_eq!(from.can_transition_to(to),
                            legal_transitions().contains(&(from, to)));
        }
    }
}
