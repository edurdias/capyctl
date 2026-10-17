//! ADR 0028 §9 (decided 2026-10-06): the completion probe's request forms.
//!
//! One probe function serves a group's readiness (1 token), the wake canary
//! (8 tokens, groups and single launches alike) and the request-stall check:
//! one non-streaming completion of a fixed prompt at temperature 0, sent by
//! the host agent to the retained launch's own loopback endpoint with its own
//! key (ADR 0012), never through ingress or the router. Each engine is asked
//! for the generated token ids in its own request form. Owner decision
//! 2026-10-09: the answer also carries the generated text, so a single
//! launch's wake canary still compares something when an engine answers no
//! token ids. A group probe still needs the token ids, except a TensorFold
//! head's (owner decision 2026-10-15, `group_activation::probe_answer`).
use serde_json::{json, Value};

use crate::traits::AdapterError;

/// The fixed prompt every completion probe sends. Exact-token comparison
/// (the wake canary) depends on it never changing between two probes.
pub const PROMPT: &str = "Say ready.";

/// The largest probe answer read; a few token ids are a small body.
pub(crate) const MAX_BODY: usize = 256 * 1024;

/// The longest generated text a probe answer keeps, in bytes. A few tokens
/// are far shorter; a longer answer is refused, never cut.
pub const MAX_TEXT: usize = 4096;

/// What one completion probe generated: the token ids when the engine
/// answered them (else empty) and the generated text (possibly empty). At
/// least one of the two is non-empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeAnswer {
    pub tokens: Vec<u32>,
    pub text: String,
}

/// Owner decision 2026-10-09: a single launch's wake canary as its engine
/// answered it, and when the answer was observed (Unix ms, on the clock of
/// whoever ran the probe: this host for an embedded engine, the host's agent
/// for a remote launch). ADR 0028 note of 2026-10-16: a wake commits its
/// evidence as observed then, since the canary can outlast the observation
/// TTL that the restore's own evidence is checked against.
#[derive(Clone, Debug, PartialEq)]
pub struct CanaryAnswer {
    pub answer: CanaryEvidence,
    pub observed_at_ms: i64,
}

/// What a wake canary's probe answered: what a chat deployment generated, or
/// (ADR 0030 §4) a decisions deployment's answer to the fixed decision probe.
/// The two are never compared with each other.
#[derive(Clone, Debug, PartialEq)]
pub enum CanaryEvidence {
    Generated(ProbeAnswer),
    Decision(crate::decision_probe::DecisionAnswer),
}

/// vLLM, TensorFold and llama.cpp: OpenAI `/v1/completions` asking for the
/// token ids (`return_token_ids`). vLLM answers them in
/// `choices[0].token_ids`; TensorFold in its own block ([`tensorfold_answer`]).
pub(crate) const OPENAI_PATH: &str = "/v1/completions";

pub(crate) fn openai_request(served: &str, max_tokens: u32) -> Value {
    json!({
        "model": served,
        "prompt": PROMPT,
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": false,
        "return_token_ids": true,
    })
}

pub(crate) fn openai_answer(answer: &Value) -> Result<ProbeAnswer, AdapterError> {
    let choice = &answer["choices"][0];
    probe_answer(&choice["token_ids"], &choice["text"])
}

/// TensorFold: the OpenAI answer, whose ids TensorFold 0.6.5 (609ca419) and
/// 0.6.6 (cb2ebf05) put in the response's own `tensorfold` block when
/// `return_token_ids` is set (`tensorfold.token_ids`, src/tensorfold/cuda/
/// server.py:558 and cuda/http.py:240-242), never in the choice. A choice's
/// `token_ids` is read when the block carries none, as vLLM's form.
pub(crate) fn tensorfold_answer(answer: &Value) -> Result<ProbeAnswer, AdapterError> {
    let choice = &answer["choices"][0];
    let ids = match &answer["tensorfold"]["token_ids"] {
        Value::Null => &choice["token_ids"],
        ids => ids,
    };
    probe_answer(ids, &choice["text"])
}

/// SGLang: its native `/generate`, answered in `output_ids`.
pub(crate) const SGLANG_PATH: &str = "/generate";

pub(crate) fn sglang_request(max_tokens: u32) -> Value {
    json!({
        "text": PROMPT,
        "sampling_params": {"max_new_tokens": max_tokens, "temperature": 0},
        "stream": false,
    })
}

pub(crate) fn sglang_answer(answer: &Value) -> Result<ProbeAnswer, AdapterError> {
    probe_answer(&answer["output_ids"], &answer["text"])
}

/// The token ids when present (a non-empty array of `u32`; present but
/// malformed is an error), and the text when present. An answer with
/// neither, or with a text longer than [`MAX_TEXT`], is an error.
fn probe_answer(ids: &Value, text: &Value) -> Result<ProbeAnswer, AdapterError> {
    let failed = |what: &str| AdapterError::Uncertain(format!("the completion answered {what}"));
    let tokens = match ids {
        Value::Null => Vec::new(),
        Value::Array(ids) => ids
            .iter()
            .map(|id| id.as_u64().and_then(|id| u32::try_from(id).ok()))
            .collect::<Option<Vec<u32>>>()
            .ok_or_else(|| failed("malformed token ids"))?,
        _ => return Err(failed("malformed token ids")),
    };
    let text = text.as_str().unwrap_or_default();
    if text.len() > MAX_TEXT {
        return Err(failed("too long a text"));
    }
    if tokens.is_empty() && text.is_empty() {
        return Err(failed("no token ids and no text"));
    }
    Ok(ProbeAnswer {
        tokens,
        text: text.to_owned(),
    })
}

/// Post `body` to `url` with `key` (none for TensorFold, ADR 0023 §3),
/// bounded by `bound`, and read the JSON answer.
pub(crate) async fn post(
    client: &reqwest::Client,
    url: reqwest::Url,
    key: Option<&reqwest::header::HeaderValue>,
    body: &Value,
    bound: std::time::Duration,
) -> Result<Value, AdapterError> {
    post_json(client, url, key, body, bound, "completion probe").await
}

/// [`post`] for any fixed probe, its errors named by `what`.
pub(crate) async fn post_json(
    client: &reqwest::Client,
    url: reqwest::Url,
    key: Option<&reqwest::header::HeaderValue>,
    body: &Value,
    bound: std::time::Duration,
    what: &str,
) -> Result<Value, AdapterError> {
    let failed = |detail: &str| AdapterError::Uncertain(format!("{what}: {detail}"));
    let mut request = client.post(url).json(body).timeout(bound);
    if let Some(key) = key {
        request = request.header(reqwest::header::AUTHORIZATION, key);
    }
    let mut response = request.send().await.map_err(|_| failed("no answer"))?;
    if !response.status().is_success() {
        return Err(failed(&format!("status {}", response.status().as_u16())));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failed("body"))? {
        if bytes.len() + chunk.len() > MAX_BODY {
            return Err(failed("body too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| failed("body is not JSON"))
}

/// A bearer header for `key`, marked sensitive so it is never formatted.
pub(crate) fn bearer(key: &str) -> Option<reqwest::header::HeaderValue> {
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    // T30 (decided 2026-10-06; text fallback, owner decision 2026-10-09):
    // each request form asks for the token ids at temperature 0 and a bounded
    // length; an answer passes with its ids, or with its text alone.
    #[test]
    fn request_forms_and_answers_are_pinned() {
        let openai = openai_request("served", 1);
        assert_eq!(openai["model"], "served");
        assert_eq!(openai["prompt"], PROMPT);
        assert_eq!(openai["max_tokens"], 1);
        assert_eq!(openai["temperature"], 0);
        assert_eq!(openai["return_token_ids"], true);
        let sglang = sglang_request(8);
        assert_eq!(sglang["text"], PROMPT);
        assert_eq!(sglang["sampling_params"]["max_new_tokens"], 8);
        assert_eq!(sglang["sampling_params"]["temperature"], 0);
        let answer = |tokens: Vec<u32>, text: &str| ProbeAnswer {
            tokens,
            text: text.into(),
        };
        assert_eq!(
            openai_answer(&json!({"choices": [{"text": "ok", "token_ids": [7, 8]}]})).unwrap(),
            answer(vec![7, 8], "ok")
        );
        assert_eq!(
            sglang_answer(&json!({"text": "ok", "output_ids": [9]})).unwrap(),
            answer(vec![9], "ok")
        );
        // An engine that answers no token ids still answers its text.
        assert_eq!(
            openai_answer(&json!({"choices": [{"text": "ok"}]})).unwrap(),
            answer(vec![], "ok")
        );
        assert_eq!(
            sglang_answer(&json!({"text": "ok"})).unwrap(),
            answer(vec![], "ok")
        );
        for bad in [
            json!({"choices": [{"token_ids": []}]}),
            json!({"choices": [{"text": "ok", "token_ids": [-1]}]}),
            json!({"choices": [{"text": "ok", "token_ids": ["7"]}]}),
            json!({"choices": [{"text": "ok", "token_ids": "7"}]}),
            json!({"choices": [{"text": "x".repeat(MAX_TEXT + 1)}]}),
            json!({}),
        ] {
            assert!(openai_answer(&bad).is_err(), "{bad}");
        }
        assert!(sglang_answer(&json!({"output_ids": []})).is_err());
    }

    // T30 (found live 2026-10-15, MN3/MN5/MN6): TensorFold answers the ids in
    // its `tensorfold` block. Both shapes are TensorFold 0.6.5's and 0.6.6's
    // non-streamed text completion (cuda/http.py:240-242): the choice carries
    // `index`, `text` and `finish_reason` only; the block carries the decode
    // stats, `token_sha`, and `token_ids` when `return_token_ids` is set.
    #[test]
    fn tensorfold_ids_come_from_its_own_block() {
        let shape = |block: Value| {
            json!({"id": "cmpl-0123456789abcdef01234567", "object": "text_completion",
                   "created": 1, "model": "tg-n35",
                   "choices": [{"index": 0, "text": " Ready", "finish_reason": "length"}],
                   "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5,
                             "prompt_tokens_details": {"cached_tokens": 0},
                             "completion_tokens_details": {"reasoning_tokens": 0}},
                   "tensorfold": block})
        };
        let stats = json!({"prefill_s": 0.0312, "cached": 0, "drafts": true, "token_sha": "5f1c"});
        let mut with_ids = stats.clone();
        with_ids["token_ids"] = json!([21872]);
        assert_eq!(
            tensorfold_answer(&shape(with_ids.clone())).unwrap(),
            ProbeAnswer {
                tokens: vec![21872],
                text: " Ready".into()
            }
        );
        // vLLM's reader finds no ids in the same answer: the live failure.
        assert!(openai_answer(&shape(with_ids)).unwrap().tokens.is_empty());
        // A build without the block's ids answers its text alone.
        assert_eq!(
            tensorfold_answer(&shape(stats)).unwrap(),
            ProbeAnswer {
                tokens: vec![],
                text: " Ready".into()
            }
        );
        // With no block, the choice's ids are read.
        assert_eq!(
            tensorfold_answer(&json!({"choices": [{"text": "ok", "token_ids": [3]}]})).unwrap(),
            ProbeAnswer {
                tokens: vec![3],
                text: "ok".into()
            }
        );
        for bad in [
            shape(json!({"token_ids": [-1]})),
            shape(json!({"token_ids": "7"})),
            json!({"choices": [{"text": ""}], "tensorfold": {"token_sha": "5f1c"}}),
        ] {
            assert!(tensorfold_answer(&bad).is_err(), "{bad}");
        }
    }
}
