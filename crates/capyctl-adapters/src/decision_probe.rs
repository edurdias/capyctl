//! ADR 0030 §3: the fixed decision probe, the readiness evidence of a
//! decisions deployment (SPEC §6.1, amended 2026-10-17).
//!
//! A decision model generates no text, so where a chat deployment proves
//! itself by generating, a decisions deployment answers one fixed
//! `POST /v1/systemone` request: a short text state and one `choice` question
//! with three options, one of which the state names. It is sent to the
//! launch's own loopback endpoint with its own key, never through ingress or
//! the router. The answer is ready evidence when it is complete and
//! well-formed ([`answer`]): the question answered as a `choice`, a
//! probability for every option and no other, each finite and within [0, 1],
//! summing to 1 within [`SUM_TOLERANCE`], and the pick one of the options. It
//! need not pick the option the state names: readiness proves the model
//! answers, not that it is right. ADR 0030 §4: the same answer is the wake
//! canary's reference, so the request never changes within a [`VERSION`].
use serde_json::{json, Map, Value};

use crate::traits::AdapterError;

/// The endpoint the probe is sent to.
pub const PATH: &str = "/v1/systemone";

/// The probe's kind and version, persisted with a canary reference so a
/// reference recorded by another request is never compared with this one.
/// A change to the request below is a new version.
pub const KIND: &str = "decision";
pub const VERSION: u32 = 1;

/// The fixed text state.
pub const STATE: &str = "The ticket says: my card was charged twice.";
/// The one question's id, instructions and options with their descriptions.
pub const QUESTION: &str = "route";
pub const INSTRUCTIONS: &str = "Which team should handle this ticket?";
pub const OPTIONS: [(&str, &str); 3] = [
    ("billing", "payments, charges and refunds"),
    ("shipping", "deliveries, parcels and tracking"),
    ("technical", "errors, crashes and sign-in problems"),
];

/// How far from 1 the probabilities may sum.
pub const SUM_TOLERANCE: f64 = 1e-3;

/// How the probe's transport errors are named.
const LABEL: &str = "decision probe";

/// A well-formed answer to the fixed probe: the option picked, and every
/// option's probability in [`OPTIONS`] order.
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionAnswer {
    pub choice: String,
    pub probabilities: Vec<(String, f64)>,
}

/// The fixed request, addressed to the engine's served model name.
pub fn request(served: &str) -> Value {
    let criteria: Map<String, Value> = OPTIONS
        .iter()
        .map(|(option, description)| ((*option).to_owned(), json!(description)))
        .collect();
    json!({
        "model": served,
        "state": STATE,
        "questions": {
            QUESTION: {
                "type": "choice",
                "instructions": INSTRUCTIONS,
                "criteria": criteria,
            }
        }
    })
}

/// ADR 0030 §3: the answer when it is complete and well-formed, else an
/// error naming what was missing (never the answer's own content).
pub fn answer(response: &Value) -> Result<DecisionAnswer, AdapterError> {
    let failed =
        |what: &str| AdapterError::Uncertain(format!("the decision probe answered {what}"));
    let answered = response
        .get("answers")
        .and_then(Value::as_object)
        .and_then(|answers| answers.get(QUESTION))
        .and_then(Value::as_object)
        .ok_or_else(|| failed("no answer to its question"))?;
    if answered.get("type").and_then(Value::as_str) != Some("choice") {
        return Err(failed("another question type"));
    }
    let given = answered
        .get("probabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| failed("no probabilities"))?;
    let probabilities = given
        .iter()
        .map(|(option, p)| {
            p.as_f64()
                .map(|p| (option.clone(), p))
                .ok_or_else(|| failed("a probability that is not a number"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let choice = answered
        .get("choice")
        .and_then(Value::as_str)
        .ok_or_else(|| failed("no pick"))?;
    well_formed(choice, &probabilities)
}

/// ADR 0030 §3: whether a pick and its probabilities are a well-formed
/// answer to the fixed question: a probability for every option and no
/// other, each finite and within [0, 1], summing to 1 within
/// [`SUM_TOLERANCE`], and the pick one of the options. Also how an answer
/// relayed by a host's agent is judged again before it counts.
pub fn well_formed(choice: &str, given: &[(String, f64)]) -> Result<DecisionAnswer, AdapterError> {
    let failed =
        |what: &str| AdapterError::Uncertain(format!("the decision probe answered {what}"));
    if given.len() != OPTIONS.len() {
        return Err(failed("probabilities for other options"));
    }
    let mut probabilities = Vec::with_capacity(OPTIONS.len());
    for (option, _) in OPTIONS {
        let p = given
            .iter()
            .find(|(answered, _)| answered == option)
            .map(|(_, p)| *p)
            .ok_or_else(|| failed("no probability for an option"))?;
        if !p.is_finite() || !(0.0..=1.0).contains(&p) {
            return Err(failed("a probability outside [0, 1]"));
        }
        probabilities.push((option.to_owned(), p));
    }
    let sum: f64 = probabilities.iter().map(|(_, p)| p).sum();
    if (sum - 1.0).abs() > SUM_TOLERANCE {
        return Err(failed("probabilities that do not sum to 1"));
    }
    if !OPTIONS.iter().any(|(option, _)| *option == choice) {
        return Err(failed("a pick that is not one of the options"));
    }
    Ok(DecisionAnswer {
        choice: choice.to_owned(),
        probabilities,
    })
}

/// Send the fixed probe to `base` with `key` (none for llama.cpp, ADR 0029
/// §4), bounded by `bound`, and judge the answer.
pub(crate) async fn probe(
    client: &reqwest::Client,
    base: &reqwest::Url,
    key: Option<&reqwest::header::HeaderValue>,
    served: &str,
    bound: std::time::Duration,
) -> Result<DecisionAnswer, AdapterError> {
    let url = base
        .join(PATH)
        .map_err(|_| AdapterError::Uncertain("decision probe: no endpoint".into()))?;
    let response =
        crate::completion_probe::post_json(client, url, key, &request(served), bound, LABEL)
            .await?;
    answer(&response)
}

/// [`probe`] for a caller holding the launch's key as text (the standalone
/// re-proof of an adopted launch, which reads it from the store), sent as a
/// bearer header marked sensitive.
pub async fn probe_keyed(
    client: &reqwest::Client,
    base: &reqwest::Url,
    key: Option<&str>,
    served: &str,
    bound: std::time::Duration,
) -> Result<DecisionAnswer, AdapterError> {
    let header = match key {
        Some(key) => Some(crate::completion_probe::bearer(key).ok_or_else(|| {
            AdapterError::Uncertain("decision probe: the key is not a header value".into())
        })?),
        None => None,
    };
    probe(client, base, header.as_ref(), served, bound).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(choice: &str, probabilities: Value) -> Value {
        json!({
            "model": "kev",
            "answers": {"route": {"type": "choice", "choice": choice,
                "probabilities": probabilities, "confidence": 0.9}},
            "usage": {"input_tokens": 40, "output_tokens": 0}
        })
    }

    // T43 (ADR 0030 §3): one choice question with three options, the state
    // naming one of them, and the instructions every engine requires.
    #[test]
    fn the_request_is_one_choice_question_with_three_options() {
        let body = request("served");
        assert_eq!(body["model"], "served");
        let question = &body["questions"][QUESTION];
        assert_eq!(question["type"], "choice");
        assert!(question["instructions"]
            .as_str()
            .is_some_and(|i| !i.is_empty()));
        let criteria = question["criteria"].as_object().unwrap();
        assert_eq!(criteria.len(), 3);
        assert!(criteria.contains_key("billing"));
    }

    // T43: a complete, well-formed answer is ready evidence whichever option
    // it picks.
    #[test]
    fn a_well_formed_answer_is_ready_whatever_it_picks() {
        let probabilities = json!({"billing": 0.9503, "shipping": 0.0157, "technical": 0.034});
        let ready = answer(&engine("billing", probabilities.clone())).unwrap();
        assert_eq!(ready.choice, "billing");
        assert_eq!(ready.probabilities.len(), 3);
        assert_eq!(
            answer(&engine("shipping", probabilities)).unwrap().choice,
            "shipping"
        );
    }

    // T43: anything incomplete or malformed is not ready.
    #[test]
    fn a_malformed_answer_is_not_ready() {
        let good = json!({"billing": 0.8, "shipping": 0.1, "technical": 0.1});
        for bad in [
            json!({"answers": {}}),
            json!({"error": {"code": 400}}),
            json!({"answers": {"route": {"type": "noul", "noul": 0.6}}}),
            engine("billing", json!({"billing": 0.8, "shipping": 0.2})),
            engine(
                "billing",
                json!({"billing": 0.8, "shipping": 0.1, "other": 0.1}),
            ),
            engine(
                "billing",
                json!({"billing": 0.8, "shipping": 0.1, "technical": 0.1, "x": 0.0}),
            ),
            engine(
                "billing",
                json!({"billing": 1.2, "shipping": -0.1, "technical": -0.1}),
            ),
            engine(
                "billing",
                json!({"billing": 0.8, "shipping": 0.1, "technical": 0.2}),
            ),
            engine(
                "billing",
                json!({"billing": "0.8", "shipping": 0.1, "technical": 0.1}),
            ),
            engine("refunds", good.clone()),
        ] {
            assert!(answer(&bad).is_err(), "{bad}");
        }
        let mut no_choice = engine("billing", good);
        no_choice["answers"]["route"]
            .as_object_mut()
            .unwrap()
            .remove("choice");
        assert!(answer(&no_choice).is_err());
    }

    // T43: a sum within 1e-3 of 1 is well formed; further is not.
    #[test]
    fn the_sum_is_one_within_a_thousandth() {
        let near = json!({"billing": 0.9505, "shipping": 0.0157, "technical": 0.0339});
        assert!(answer(&engine("billing", near)).is_ok());
        let far = json!({"billing": 0.9525, "shipping": 0.0157, "technical": 0.0339});
        assert!(answer(&engine("billing", far)).is_err());
    }
}
