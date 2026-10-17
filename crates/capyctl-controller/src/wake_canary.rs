//! Owner decision 2026-10-09: a single launch's wake canary.
//!
//! The group canary (ADR 0028 §12) for one launch on one host. At the
//! launch's first readiness the controller asks the engine for one greedy
//! completion of the fixed probe prompt ([`CANARY_TOKENS`] tokens at
//! temperature 0, through [`EngineAdapter::wake_canary`]) and records what it
//! generated, durably, for that launch alone. After every wake it asks the
//! same and compares the two with [`matches`]: the token ids by the group's
//! own rule ([`canary_matches`]) when both answers carry them, the generated
//! text otherwise. A wake whose answer differs fails with
//! [`WAKE_MISMATCH`] and the instance is stopped under the engine-exit
//! principal, its status naming the code as its last error. A launch that
//! recorded no reference (its probe failed at readiness) records the first
//! wake's answer instead of comparing, as a group does. CPU and fake-engine
//! tests only cover this; a live deep park and wake on a host qualifies it.
//!
//! ADR 0030 §4: a decisions deployment's canary is its fixed decision probe
//! (`capyctl_adapters::decision_probe`). Its reference keeps the probe's
//! version, the option picked and every option's probability, and the rule
//! it is compared by ([`decision_matches`]): the same option, and every
//! probability within [`DECISION_TOLERANCE`] of the reference's. A reference
//! of one kind or version is never compared with an answer of another: that
//! answer finds no usable reference.
//!
//! [`EngineAdapter::wake_canary`]: capyctl_adapters::traits::EngineAdapter::wake_canary
use crate::group_residency::{canary_matches, CanaryReference};
use capyctl_adapters::completion_probe::{CanaryEvidence, ProbeAnswer, PROMPT};
use capyctl_adapters::decision_probe::{self, DecisionAnswer};
use capyctl_store::wake_canary::{StoredDecisionCanary, StoredWakeCanary, WakeCanaryReference};

pub use crate::group_residency::{CANARY_DEADLINE, CANARY_TOKENS};
pub use capyctl_store::wake_canary::{
    DECISION_TOLERANCE, WAKE_MISMATCH, WAKE_RELOAD_FAILED, WAKE_UNCERTAIN,
};

/// What a wake's canary proves against its launch's reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The answer matches the reference.
    Matches,
    /// The answer differs from the reference: the wake fails `wake_mismatch`.
    Differs,
    /// No usable reference (none recorded, or one recorded for another
    /// prompt, probe kind or probe version): the answer becomes the
    /// reference.
    Unreferenced,
}

/// The reference a probe answer records. ADR 0030 §4: a decision answer
/// records its probe's version and the comparison rule with it.
pub fn reference(answer: &CanaryEvidence) -> WakeCanaryReference {
    match answer {
        CanaryEvidence::Generated(answer) => WakeCanaryReference::Completion(StoredWakeCanary {
            prompt: PROMPT.to_owned(),
            tokens: answer.tokens.clone(),
            text: answer.text.clone(),
        }),
        CanaryEvidence::Decision(answer) => WakeCanaryReference::Decision(StoredDecisionCanary {
            probe_version: decision_probe::VERSION,
            choice: answer.choice.clone(),
            probabilities: answer.probabilities.clone(),
            tolerance: DECISION_TOLERANCE,
        }),
    }
}

/// Owner decision 2026-10-09: judge a wake's canary `observed` against the
/// launch's `reference`. Token ids are compared under the group rule when
/// both carry them; otherwise the generated text, when both carry it. An
/// answer that shares neither form with its reference cannot be shown to
/// match and differs. ADR 0030 §4: a decision answer is compared only with a
/// decision reference of the same probe version, by [`decision_matches`].
pub fn verdict(reference: Option<&WakeCanaryReference>, observed: &CanaryEvidence) -> Verdict {
    let matched = match (reference, observed) {
        (Some(WakeCanaryReference::Completion(reference)), CanaryEvidence::Generated(observed))
            if reference.prompt == PROMPT =>
        {
            matches(reference, observed)
        }
        (Some(WakeCanaryReference::Decision(reference)), CanaryEvidence::Decision(observed))
            if reference.probe_version == decision_probe::VERSION =>
        {
            decision_matches(reference, observed)
        }
        _ => return Verdict::Unreferenced,
    };
    if matched {
        Verdict::Matches
    } else {
        Verdict::Differs
    }
}

/// The one comparison of a single launch's canary with its reference.
pub fn matches(reference: &StoredWakeCanary, observed: &ProbeAnswer) -> bool {
    if !reference.tokens.is_empty() && !observed.tokens.is_empty() {
        return canary_matches(
            &CanaryReference {
                prompt: reference.prompt.clone(),
                tokens: reference.tokens.clone(),
            },
            &observed.tokens,
        );
    }
    !reference.text.is_empty() && reference.text == observed.text
}

/// ADR 0030 §4: the comparison rule of a decision reference. The same option
/// picked, the same options answered, and every probability within the
/// reference's recorded tolerance of its own. Probabilities move slightly
/// with batching and caching, so exact equality is not the rule; the bound
/// is inclusive up to floating-point rounding of the difference.
pub fn decision_matches(reference: &StoredDecisionCanary, observed: &DecisionAnswer) -> bool {
    const ROUNDING: f64 = 1e-9;
    reference.choice == observed.choice
        && reference.probabilities.len() == observed.probabilities.len()
        && reference.probabilities.iter().all(|(option, expected)| {
            observed
                .probabilities
                .iter()
                .find(|(answered, _)| answered == option)
                .is_some_and(|(_, p)| (p - expected).abs() <= reference.tolerance + ROUNDING)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(tokens: &[u32], text: &str) -> CanaryEvidence {
        CanaryEvidence::Generated(ProbeAnswer {
            tokens: tokens.to_vec(),
            text: text.into(),
        })
    }

    fn decided(choice: &str, billing: f64, shipping: f64, technical: f64) -> CanaryEvidence {
        CanaryEvidence::Decision(DecisionAnswer {
            choice: choice.into(),
            probabilities: vec![
                ("billing".into(), billing),
                ("shipping".into(), shipping),
                ("technical".into(), technical),
            ],
        })
    }

    // T20 (owner decision 2026-10-09): token ids decide when both answers
    // carry them, exactly as the group canary; the text otherwise.
    #[test]
    fn tokens_decide_and_text_is_the_fallback() {
        let both = reference(&answer(&[1, 2, 3], "Ready."));
        assert_eq!(
            verdict(Some(&both), &answer(&[1, 2, 3], "Ready.")),
            Verdict::Matches
        );
        // Same text, other tokens: the tokens decide.
        assert_eq!(
            verdict(Some(&both), &answer(&[1, 2, 4], "Ready.")),
            Verdict::Differs
        );
        assert_eq!(
            verdict(Some(&both), &answer(&[1, 2], "Ready.")),
            Verdict::Differs
        );
        // An engine that answers no token ids is compared by its text.
        assert_eq!(
            verdict(Some(&both), &answer(&[], "Ready.")),
            Verdict::Matches
        );
        assert_eq!(verdict(Some(&both), &answer(&[], "!!!!")), Verdict::Differs);
        let text_only = reference(&answer(&[], "Ready."));
        assert_eq!(
            verdict(Some(&text_only), &answer(&[7], "Ready.")),
            Verdict::Matches
        );
        // Nothing in common to compare: it cannot be shown to match.
        let tokens_only = reference(&answer(&[1], ""));
        assert_eq!(
            verdict(Some(&tokens_only), &answer(&[], "Ready.")),
            Verdict::Differs
        );
    }

    // T20: no reference, or one for another prompt, records instead.
    #[test]
    fn an_unusable_reference_records_instead() {
        assert_eq!(verdict(None, &answer(&[1], "")), Verdict::Unreferenced);
        let WakeCanaryReference::Completion(mut other) = reference(&answer(&[1], "")) else {
            unreachable!()
        };
        other.prompt = "another prompt".into();
        assert_eq!(
            verdict(
                Some(&WakeCanaryReference::Completion(other)),
                &answer(&[9], "")
            ),
            Verdict::Unreferenced
        );
    }

    // T43 T20 (ADR 0030 §4): the same pick with every probability within
    // 0.05 of the reference passes; a changed pick, or any probability moved
    // by more, differs.
    #[test]
    fn a_decision_matches_within_tolerance_and_differs_beyond_it() {
        let recorded = reference(&decided("billing", 0.9503, 0.0157, 0.0340));
        for close in [
            decided("billing", 0.9503, 0.0157, 0.0340),
            decided("billing", 0.9493, 0.0167, 0.0340),
            decided("billing", 0.9003, 0.0657, 0.0340),
        ] {
            assert_eq!(verdict(Some(&recorded), &close), Verdict::Matches);
        }
        for far in [
            decided("shipping", 0.9503, 0.0157, 0.0340),
            decided("billing", 0.8990, 0.0670, 0.0340),
            decided("billing", 0.8903, 0.0157, 0.0940),
        ] {
            assert_eq!(verdict(Some(&recorded), &far), Verdict::Differs);
        }
        // An answer naming other options cannot be shown to match.
        let CanaryEvidence::Decision(mut renamed) = decided("billing", 0.9503, 0.0157, 0.0340)
        else {
            unreachable!()
        };
        renamed.probabilities[2].0 = "other".into();
        assert_eq!(
            verdict(Some(&recorded), &CanaryEvidence::Decision(renamed)),
            Verdict::Differs
        );
    }

    // T43 (ADR 0030 §4): a reference of one kind is never compared with an
    // answer of the other, nor a decision reference of another probe version.
    #[test]
    fn kinds_and_versions_are_never_compared_across() {
        let chat = reference(&answer(&[1, 2, 3], "Ready."));
        assert_eq!(
            verdict(Some(&chat), &decided("billing", 0.9, 0.05, 0.05)),
            Verdict::Unreferenced
        );
        let decision = reference(&decided("billing", 0.9, 0.05, 0.05));
        assert_eq!(
            verdict(Some(&decision), &answer(&[1, 2, 3], "Ready.")),
            Verdict::Unreferenced
        );
        let WakeCanaryReference::Decision(mut older) = decision else {
            unreachable!()
        };
        older.probe_version = decision_probe::VERSION + 1;
        assert_eq!(
            verdict(
                Some(&WakeCanaryReference::Decision(older)),
                &decided("technical", 0.0, 0.0, 1.0)
            ),
            Verdict::Unreferenced
        );
    }
}
