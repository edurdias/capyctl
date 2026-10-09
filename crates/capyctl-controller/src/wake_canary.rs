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
//! [`EngineAdapter::wake_canary`]: capyctl_adapters::traits::EngineAdapter::wake_canary
use crate::group_residency::{canary_matches, CanaryReference};
use capyctl_adapters::completion_probe::{ProbeAnswer, PROMPT};
use capyctl_store::wake_canary::StoredWakeCanary;

pub use crate::group_residency::{CANARY_DEADLINE, CANARY_TOKENS};
pub use capyctl_store::wake_canary::WAKE_MISMATCH;

/// What a wake's canary proves against its launch's reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The answer matches the reference.
    Matches,
    /// The answer differs from the reference: the wake fails `wake_mismatch`.
    Differs,
    /// No usable reference (none recorded, or one recorded for another
    /// prompt): the answer becomes the reference.
    Unreferenced,
}

/// The reference a probe answer records.
pub fn reference(answer: &ProbeAnswer) -> StoredWakeCanary {
    StoredWakeCanary {
        prompt: PROMPT.to_owned(),
        tokens: answer.tokens.clone(),
        text: answer.text.clone(),
    }
}

/// Owner decision 2026-10-09: judge a wake's canary `observed` against the
/// launch's `reference`. Token ids are compared under the group rule when
/// both carry them; otherwise the generated text, when both carry it. An
/// answer that shares neither form with its reference cannot be shown to
/// match and differs.
pub fn verdict(reference: Option<&StoredWakeCanary>, observed: &ProbeAnswer) -> Verdict {
    match reference {
        Some(reference) if reference.prompt == PROMPT => {
            if matches(reference, observed) {
                Verdict::Matches
            } else {
                Verdict::Differs
            }
        }
        _ => Verdict::Unreferenced,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(tokens: &[u32], text: &str) -> ProbeAnswer {
        ProbeAnswer {
            tokens: tokens.to_vec(),
            text: text.into(),
        }
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
        let mut other = reference(&answer(&[1], ""));
        other.prompt = "another prompt".into();
        assert_eq!(
            verdict(Some(&other), &answer(&[9], "")),
            Verdict::Unreferenced
        );
    }
}
