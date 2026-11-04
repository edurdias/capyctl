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
//! ADR 0036 §6: an embeddings deployment's canary is its fixed embedding
//! probe (`capyctl_adapters::embedding_probe`). Its reference keeps the
//! probe's kind and version, the vector's dimension, the vector and the
//! tolerance ([`EMBEDDING_TOLERANCE`]); a later answer matches when its
//! dimension is the same and its relative distance `‖v − r‖ / ‖r‖` is within
//! the tolerance (`compare_embedding_canary`).
//!
//! ADR 0041 §4: an extraction reference keeps the probe kind, version,
//! declared types and complete answers. ADR 0041 §5: a later answer uses
//! the shared extraction probe comparison, never another type set.
//!
//! [`EngineAdapter::wake_canary`]: capyctl_adapters::traits::EngineAdapter::wake_canary
use crate::group_residency::{canary_matches, CanaryReference};
use capyctl_adapters::completion_probe::{CanaryEvidence, ProbeAnswer, PROMPT};
use capyctl_adapters::decision_probe::{self, DecisionAnswer};
use capyctl_adapters::embedding_probe;
use capyctl_adapters::extraction_probe;
use capyctl_store::wake_canary::{
    compare_embedding_canary, EmbeddingCanaryOutcome, StoredDecisionCanary, StoredEmbeddingCanary,
    StoredExtractionCanary, StoredWakeCanary, WakeCanaryReference,
};

pub use crate::group_residency::{CANARY_DEADLINE, CANARY_TOKENS};
pub use capyctl_store::wake_canary::{
    DECISION_TOLERANCE, WAKE_MISMATCH, WAKE_RELOAD_FAILED, WAKE_UNCERTAIN,
};

/// ADR 0036 §6 (owner decision 2026-10-27, question 3): the relative
/// distance an embedding canary may move by and still match. Provisional:
/// row EM3 sets the figure (its largest measured distance plus a stated
/// margin), and 0.02 is the figure EM3 is checked against (cosine 0.9998 for
/// unit vectors); EM3's figure replaces this one.
pub const EMBEDDING_TOLERANCE: f64 = 0.02;

/// What a wake's canary proves against its launch's reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The answer matches the reference.
    Matches,
    /// The answer differs from the reference: the wake fails `wake_mismatch`.
    Differs,
    /// No usable reference (none recorded, or one recorded for another
    /// prompt, probe kind, probe version or declared extraction type set):
    /// the answer becomes the reference.
    Unreferenced,
}

/// The reference a probe answer records. ADR 0030 §4: a decision answer
/// records its probe's version and the comparison rule with it; ADR 0036 §6
/// an embedding answer its probe's kind and version, its dimension, the
/// vector and the tolerance. ADR 0041 §4: an extraction answer keeps its
/// probe identity, declared types and complete answers.
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
        CanaryEvidence::Embedding(answer) => {
            WakeCanaryReference::Embedding(StoredEmbeddingCanary {
                probe_kind: embedding_probe::KIND.to_owned(),
                probe_version: embedding_probe::VERSION,
                // A vector longer than u32 dimensions is not one an engine
                // answers; saturating keeps it from ever matching a reference.
                dimension: u32::try_from(answer.dimension()).unwrap_or(u32::MAX),
                vector: answer.vector.clone(),
                tolerance: EMBEDDING_TOLERANCE,
            })
        }
        // ADR 0041 §4: these types and answers belong to this probe identity.
        CanaryEvidence::Extraction(answer) => {
            WakeCanaryReference::Extraction(StoredExtractionCanary {
                probe_kind: extraction_probe::KIND.to_owned(),
                probe_version: extraction_probe::VERSION,
                declared: answer.declared.clone(),
                answers: answer.answers.clone(),
            })
        }
    }
}

/// Owner decision 2026-10-09: judge a wake's canary `observed` against the
/// launch's `reference`. Token ids are compared under the group rule when
/// both carry them; otherwise the generated text, when both carry it. An
/// answer that shares neither form with its reference cannot be shown to
/// match and differs. ADR 0030 §4: a decision answer is compared only with a
/// decision reference of the same probe version, by [`decision_matches`].
/// ADR 0036 §6: an embedding answer only with an embedding reference of the
/// same probe kind and version, by `compare_embedding_canary`.
/// ADR 0041 §4: an extraction answer only with the same probe identity and
/// declared types, using ADR 0041 §5's shared comparison.
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
        (Some(WakeCanaryReference::Embedding(reference)), CanaryEvidence::Embedding(observed)) => {
            match compare_embedding_canary(
                reference,
                embedding_probe::KIND,
                embedding_probe::VERSION,
                &observed.vector,
            ) {
                EmbeddingCanaryOutcome::Matches => true,
                EmbeddingCanaryOutcome::Differs => false,
                EmbeddingCanaryOutcome::NotComparable => return Verdict::Unreferenced,
            }
        }
        // ADR 0041 §4: an answer for a different probe identity or declared
        // type set records a new reference instead of comparing across them.
        (
            Some(WakeCanaryReference::Extraction(reference)),
            CanaryEvidence::Extraction(observed),
        ) if reference.probe_kind == extraction_probe::KIND
            && reference.probe_version == extraction_probe::VERSION
            && reference.declared == observed.declared =>
        {
            // ADR 0041 §5: discrete guesses repeat; distributions may move
            // one probe step, and probabilities by the inclusive 0.05.
            extraction_probe::matches(
                &reference.declared,
                &reference.answers,
                observed,
                DECISION_TOLERANCE,
            )
            .is_ok()
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

    fn embedded(vector: &[f32]) -> CanaryEvidence {
        CanaryEvidence::Embedding(capyctl_adapters::embedding_probe::EmbeddingAnswer {
            vector: vector.to_vec(),
        })
    }

    // T47 T20 (ADR 0036 §6): the first ready embedding is recorded with the
    // probe's kind and version, its dimension, the exact vector and the
    // tolerance checked against 0.02.
    #[test]
    fn an_embedding_records_its_vector_and_tolerance() {
        let WakeCanaryReference::Embedding(recorded) = reference(&embedded(&[0.6, 0.0, -0.8]))
        else {
            panic!("an embedding answer records an embedding reference");
        };
        assert_eq!(
            recorded,
            StoredEmbeddingCanary {
                probe_kind: "embedding".into(),
                probe_version: 1,
                dimension: 3,
                vector: vec![0.6, 0.0, -0.8],
                tolerance: 0.02,
            }
        );
        assert_eq!(verdict(None, &embedded(&[0.6, 0.8])), Verdict::Unreferenced);
    }

    // T47 T20 (ADR 0036 §6): a vector within a relative distance of 0.02 of
    // the reference passes; one beyond it, or of another dimension, differs
    // (`wake_mismatch`).
    #[test]
    fn an_embedding_matches_within_tolerance_and_differs_beyond_it() {
        let recorded = reference(&embedded(&[0.6, 0.8]));
        for close in [&[0.6, 0.8][..], &[0.619, 0.8], &[0.6, 0.781]] {
            assert_eq!(
                verdict(Some(&recorded), &embedded(close)),
                Verdict::Matches,
                "{close:?}"
            );
        }
        for far in [
            &[0.621, 0.8][..],
            &[0.8, 0.6],
            &[-0.6, -0.8],
            &[0.6, 0.8, 0.0],
            &[0.6],
        ] {
            assert_eq!(
                verdict(Some(&recorded), &embedded(far)),
                Verdict::Differs,
                "{far:?}"
            );
        }
    }

    // T47 (ADR 0036 §5): an embedding reference of another probe kind or
    // version is never compared, nor an embedding with a reference of
    // another kind of probe.
    #[test]
    fn embedding_references_are_never_compared_across() {
        let chat = reference(&answer(&[1, 2, 3], "Ready."));
        let decision = reference(&decided("billing", 0.9, 0.05, 0.05));
        for other in [&chat, &decision] {
            assert_eq!(
                verdict(Some(other), &embedded(&[0.6, 0.8])),
                Verdict::Unreferenced
            );
        }
        let recorded = reference(&embedded(&[0.6, 0.8]));
        assert_eq!(
            verdict(Some(&recorded), &answer(&[1, 2, 3], "Ready.")),
            Verdict::Unreferenced
        );
        let WakeCanaryReference::Embedding(older) = recorded else {
            unreachable!()
        };
        let mut later = older.clone();
        later.probe_version = capyctl_adapters::embedding_probe::VERSION + 1;
        let mut kind = older;
        kind.probe_kind = "decision".into();
        for unusable in [later, kind] {
            assert_eq!(
                verdict(
                    Some(&WakeCanaryReference::Embedding(unusable)),
                    &embedded(&[0.0, 1.0])
                ),
                Verdict::Unreferenced
            );
        }
    }

    fn extracted() -> capyctl_adapters::extraction_probe::ExtractionAnswer {
        use capyctl_config::extractions::FieldType;
        let body = serde_json::json!({
            "answers": {
                "amount_paid": {
                    "type": "number", "value": 649.0, "guess": 649.0,
                    "p_null": 0.25, "confidence": 0.9
                },
                "delivered_on": {
                    "type": "date", "value": "2026-09-28", "guess": "2026-09-28",
                    "p_null": 0.25, "confidence": 0.9
                },
                "returns_today": {
                    "type": "estimate", "value": 13, "median": 13, "p10": 11, "p90": 15,
                    "p_below": 0.1, "p_above": 0.1
                },
                "returns_next": {
                    "type": "forecast", "value": [13, 14], "median": [13, 14],
                    "p10": [11, 12], "p90": [15, 16], "p_below": 0.1, "p_above": 0.1,
                    "steps": [
                        {"value": 13, "median": 13, "p10": 11, "p90": 15, "p_below": 0.1},
                        {"value": 14, "median": 14, "p10": 12, "p90": 16, "p_above": 0.1}
                    ]
                }
            }
        });
        capyctl_adapters::extraction_probe::well_formed(
            &body,
            &[
                FieldType::Number,
                FieldType::Date,
                FieldType::Estimate,
                FieldType::Forecast,
            ],
        )
        .unwrap()
    }

    // T50: ADR 0041 §4: conversion preserves the complete first answer and
    // records the probe kind, version and canonical declared type set.
    #[test]
    fn an_extraction_records_its_probe_identity_and_declared_answers() {
        let answer = extracted();
        let WakeCanaryReference::Extraction(recorded) =
            reference(&CanaryEvidence::Extraction(answer.clone()))
        else {
            panic!("an extraction records an extraction reference");
        };
        assert_eq!(recorded.probe_kind, "extraction");
        assert_eq!(recorded.probe_version, 1);
        assert_eq!(recorded.declared, answer.declared);
        assert_eq!(recorded.answers, answer.answers);
    }

    // T50: ADR 0041 §5: the shared comparison includes the one-step and
    // 0.05 boundaries, and compares every forecast representation recorded.
    #[test]
    fn extraction_tolerances_are_inclusive_and_changes_beyond_them_fail() {
        let mut initial = extracted();
        // Compare exact binary differences: decimal 0.9 - 0.85 is slightly
        // larger than 0.05 and must not widen the production tolerance.
        initial.answers["amount_paid"]["p_null"] = 0.0.into();
        initial.answers["amount_paid"]["confidence"] = 0.0.into();
        initial.answers["returns_today"]["p_below"] = 0.0.into();
        initial.answers["returns_next"]["p_above"] = 0.0.into();
        initial.answers["returns_next"]["steps"][0]["p_below"] = 0.0.into();
        let recorded = reference(&CanaryEvidence::Extraction(initial.clone()));
        assert_eq!(
            verdict(
                Some(&recorded),
                &CanaryEvidence::Extraction(initial.clone())
            ),
            Verdict::Matches
        );
        let mut close = initial.clone();
        close.answers["amount_paid"]["p_null"] = 0.05.into();
        close.answers["amount_paid"]["confidence"] = 0.05.into();
        // Whether the scalar's value is null is not part of wake comparison.
        close.answers["amount_paid"]["value"] = serde_json::Value::Null;
        close.answers["returns_today"]["p_below"] = 0.05.into();
        close.answers["returns_next"]["p_above"] = 0.05.into();
        close.answers["returns_next"]["steps"][0]["p_below"] = 0.05.into();
        for member in ["value", "median", "p10", "p90"] {
            let estimate = &mut close.answers["returns_today"][member];
            *estimate = (estimate.as_i64().unwrap() + 1).into();
            for value in close.answers["returns_next"][member]
                .as_array_mut()
                .unwrap()
            {
                *value = (value.as_i64().unwrap() + 1).into();
            }
            for step in close.answers["returns_next"]["steps"]
                .as_array_mut()
                .unwrap()
            {
                step[member] = (step[member].as_i64().unwrap() + 1).into();
            }
        }
        assert_eq!(
            verdict(Some(&recorded), &CanaryEvidence::Extraction(close)),
            Verdict::Matches
        );
        let mut probability = initial.clone();
        probability.answers["amount_paid"]["p_null"] = f64::from_bits(0.05f64.to_bits() + 1).into();
        let mut estimate = initial.clone();
        estimate.answers["returns_today"]["value"] = 15.into();
        let mut forecast = initial;
        forecast.answers["returns_next"]["steps"][0]["value"] = 15.into();
        for far in [probability, estimate, forecast] {
            assert_eq!(
                verdict(Some(&recorded), &CanaryEvidence::Extraction(far)),
                Verdict::Differs
            );
        }
    }

    // T50: ADR 0041 §5: a well-formed changed scalar guess fails even when
    // its probabilities are unchanged, for both numeric and discrete types.
    #[test]
    fn extraction_changed_guesses_fail() {
        let initial = extracted();
        let recorded = reference(&CanaryEvidence::Extraction(initial.clone()));
        let mut number = initial.clone();
        number.answers["amount_paid"]["guess"] = 649.01.into();
        number.answers["amount_paid"]["value"] = 649.01.into();
        let mut date = initial;
        date.answers["delivered_on"]["guess"] = "2026-09-29".into();
        date.answers["delivered_on"]["value"] = "2026-09-29".into();
        for changed in [number, date] {
            assert_eq!(
                verdict(Some(&recorded), &CanaryEvidence::Extraction(changed)),
                Verdict::Differs
            );
        }
    }

    // T50: ADR 0041 §4: a reference for a different kind, version or
    // declared type set is unreferenced, never compared with this answer.
    #[test]
    fn extraction_references_are_never_compared_across() {
        let initial = extracted();
        let observed = CanaryEvidence::Extraction(initial.clone());
        assert_eq!(verdict(None, &observed), Verdict::Unreferenced);
        let recorded = reference(&observed);
        for other in [
            reference(&answer(&[1], "Ready.")),
            reference(&decided("billing", 0.9, 0.05, 0.05)),
            reference(&embedded(&[1.0])),
        ] {
            assert_eq!(verdict(Some(&other), &observed), Verdict::Unreferenced);
        }
        assert_eq!(
            verdict(Some(&recorded), &answer(&[1], "Ready.")),
            Verdict::Unreferenced
        );
        let WakeCanaryReference::Extraction(original) = recorded else {
            unreachable!()
        };
        let mut kind = original.clone();
        kind.probe_kind = "decision".into();
        let mut version = original.clone();
        version.probe_version += 1;
        let mut declared = original;
        declared
            .declared
            .retain(|kind| *kind != capyctl_config::extractions::FieldType::Forecast);
        declared.answers.remove("returns_next");
        for unusable in [kind, version, declared] {
            assert_eq!(
                verdict(Some(&WakeCanaryReference::Extraction(unusable)), &observed),
                Verdict::Unreferenced
            );
        }
    }
}
