//! ADR 0036 §3-§4: the embeddings request contract, judged before anything
//! is sent to an engine, and completion evidence for a whole answer.
use crate::embedding_answer::{AnswerParser, Encoding, Expected};
use crate::traits::{AdapterError, EmbeddingAnswered};
use serde_json::Value;

/// ADR 0036 §3, SPEC §10: the top-level fields of an embeddings request
/// (`POST /v1/embeddings`), OpenAI's contract. The list is closed: an engine
/// extension (vLLM's `truncate_prompt_tokens` or `embed_dtype`, SGLang's
/// `rid` or `lora_path`, llama.cpp's `content` or `embd_normalize`),
/// `stream` and `cache_salt` (owner decision 2026-10-27, question 10) are
/// refused, never forwarded.
pub const EMBEDDING_REQUEST_FIELDS: &[&str] =
    &["model", "input", "encoding_format", "dimensions", "user"];

/// ADR 0036 §3: the most inputs one embeddings request carries.
pub const MAX_EMBEDDING_INPUTS: usize = 2048;

/// ADR 0036 §3: the largest token ID an input may hold, 2³¹−1.
pub const MAX_TOKEN_ID: u64 = (1 << 31) - 1;

/// Why an embeddings request is refused before anything is sent to an engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbeddingRequestRefusal {
    /// Not a JSON object, no non-empty `model`, or a field of the wrong shape.
    Malformed(&'static str),
    /// A top-level field outside [`EMBEDDING_REQUEST_FIELDS`]. Refused, never
    /// dropped.
    Field(String),
    /// ADR 0036 §3 (owner decision 2026-10-27, question 4): llama.cpp ignores
    /// `dimensions`, so on llama.cpp only its native dimension is accepted.
    Dimensions { native: usize },
}

impl std::fmt::Display for EmbeddingRequestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed embeddings request: {what}"),
            Self::Field(name) => write!(f, "embeddings request field `{name}` is not accepted"),
            Self::Dimensions { native } => write!(
                f,
                "dimensions must be {native}: llama.cpp returns full vectors of the model's native dimension"
            ),
        }
    }
}

impl From<EmbeddingRequestRefusal> for AdapterError {
    fn from(refusal: EmbeddingRequestRefusal) -> Self {
        match refusal {
            EmbeddingRequestRefusal::Field(_) => AdapterError::PolicyDenied,
            EmbeddingRequestRefusal::Malformed(_) | EmbeddingRequestRefusal::Dimensions { .. } => {
                AdapterError::UnsupportedCapability
            }
        }
    }
}

/// ADR 0036 §3: validate an embeddings request against its closed contract
/// before anything is sent, so every refusal is a deterministic client error
/// with nothing reaching an engine (SPEC §10, T47). Shapes only: whether a
/// token ID is in the vocabulary, whether an input fits the model's length
/// and whether the model supports a `dimensions` are the engine's to judge.
pub fn validate_embedding_request(body: &Value) -> Result<(), EmbeddingRequestRefusal> {
    embedding_expected(body).map(drop)
}

/// Whether `body` is an embeddings request the host ingress may forward.
pub fn embedding_request_allowed(body: &Value) -> bool {
    validate_embedding_request(body).is_ok()
}

/// ADR 0036 §3 (owner decision 2026-10-27, question 4): on llama.cpp, which
/// ignores `dimensions` and returns full vectors, a request asking for other
/// than `native` (the dimension recorded at readiness, §5) is refused. The
/// router calls this for llama.cpp deployments only; vLLM and SGLang judge
/// `dimensions` themselves.
pub fn validate_native_dimensions(
    body: &Value,
    native: usize,
) -> Result<(), EmbeddingRequestRefusal> {
    match body.get("dimensions").and_then(Value::as_u64) {
        Some(asked) if usize::try_from(asked) != Ok(native) => {
            Err(EmbeddingRequestRefusal::Dimensions { native })
        }
        _ => Ok(()),
    }
}

/// ADR 0036 §3: what a valid request makes its answer owe.
pub(crate) fn embedding_expected(body: &Value) -> Result<Expected, EmbeddingRequestRefusal> {
    use EmbeddingRequestRefusal::Malformed;
    let Some(fields) = body.as_object() else {
        return Err(Malformed("the body is not a JSON object"));
    };
    if let Some(name) = fields
        .keys()
        .find(|name| !EMBEDDING_REQUEST_FIELDS.contains(&name.as_str()))
    {
        return Err(EmbeddingRequestRefusal::Field(name.clone()));
    }
    if !fields
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| !model.is_empty())
    {
        return Err(Malformed("model is required"));
    }
    let token_ids = |ids: &[Value]| {
        ids.iter()
            .all(|id| id.as_u64().is_some_and(|id| id <= MAX_TOKEN_ID))
    };
    let inputs = match fields.get("input") {
        Some(Value::String(text)) if !text.is_empty() => 1,
        Some(Value::Array(items)) if !items.is_empty() => {
            let batch = match &items[0] {
                // A token array is one input, however many tokens it holds.
                Value::Number(_) => false,
                Value::String(_) | Value::Array(_) => true,
                _ => {
                    return Err(Malformed(
                        "each input must be a string, a token ID or an array of token IDs",
                    ))
                }
            };
            if batch && items.len() > MAX_EMBEDDING_INPUTS {
                return Err(Malformed("input holds more than 2048 inputs"));
            }
            let shaped = match &items[0] {
                Value::Number(_) => token_ids(items),
                Value::String(_) => items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|text| !text.is_empty())),
                _ => items.iter().all(|item| {
                    item.as_array()
                        .is_some_and(|ids| !ids.is_empty() && token_ids(ids))
                }),
            };
            if !shaped {
                return Err(Malformed(
                    "input must be non-empty strings, token IDs from 0 to 2147483647 or non-empty arrays of them, never mixed",
                ));
            }
            if batch {
                items.len()
            } else {
                1
            }
        }
        _ => {
            return Err(Malformed(
                "input is required: a non-empty string or a non-empty array",
            ))
        }
    };
    let encoding = match fields.get("encoding_format").map(Value::as_str) {
        None | Some(Some("float")) => Encoding::Float,
        Some(Some("base64")) => Encoding::Base64,
        Some(_) => return Err(Malformed("encoding_format must be float or base64")),
    };
    let dimensions = match fields.get("dimensions") {
        None => None,
        Some(dimensions) => Some(
            dimensions
                .as_u64()
                .filter(|dimensions| *dimensions > 0)
                .and_then(|dimensions| usize::try_from(dimensions).ok())
                .ok_or(Malformed("dimensions must be a positive integer"))?,
        ),
    };
    if fields.get("user").is_some_and(|user| !user.is_string()) {
        return Err(Malformed("user must be a string"));
    }
    Ok(Expected {
        inputs,
        encoding,
        dimensions,
    })
}

/// ADR 0036 §4: completion evidence for a whole embeddings answer to
/// `request`: the same checks the streamed answer passes through
/// (`ChatForward::forward_embedding`), on bytes already read. `None` when the request is
/// not valid or the answer fails a check.
pub fn embedding_answered(request: &Value, answer: &[u8]) -> Option<EmbeddingAnswered> {
    let expected = embedding_expected(request).ok()?;
    let mut parser = AnswerParser::new(expected, "");
    let mut relayed = Vec::new();
    for chunk in answer.chunks(64 * 1024) {
        relayed.clear();
        parser.feed(chunk, &mut relayed).ok()?;
    }
    let complete = parser.finish().ok()?;
    Some(EmbeddingAnswered {
        inputs: expected.inputs,
        prompt_tokens: complete.prompt_tokens,
        client_gone: false,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{json, Value};

    pub(crate) fn embeddings(extra: Value) -> Value {
        let mut body = json!({"model": "public", "input": "a quiet library"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    }

    /// ADR 0036 §3: an embeddings request is held to OpenAI's closed contract
    /// before anything is sent: each input shape is accepted, every other
    /// field is refused by name, and empty, mixed, oversized or out-of-range
    /// inputs are refused.
    // T47
    #[test]
    fn an_embeddings_request_is_held_to_its_own_closed_contract() {
        use super::{
            embedding_expected, embedding_request_allowed, validate_embedding_request as validate,
            EmbeddingRequestRefusal as Refusal, MAX_EMBEDDING_INPUTS, MAX_TOKEN_ID,
        };
        use crate::embedding_answer::Encoding;
        use crate::forward::tests::decision;
        use crate::forward::{validate_chat_request, validate_decision_request};
        use crate::traits::AdapterError;
        let texts = |n: usize| Value::Array((0..n).map(|i| json!(format!("text {i}"))).collect());
        let token_arrays = |n: usize| Value::Array((0..n).map(|_| json!([1, 2])).collect());
        for (input, inputs) in [
            (json!("one"), 1),
            (json!(["one", "two"]), 2),
            (texts(MAX_EMBEDDING_INPUTS), MAX_EMBEDDING_INPUTS),
            (json!([0, 17, MAX_TOKEN_ID]), 1),
            (json!([[0], [17, MAX_TOKEN_ID]]), 2),
            (token_arrays(MAX_EMBEDDING_INPUTS), MAX_EMBEDDING_INPUTS),
        ] {
            let body = embeddings(json!({ "input": input }));
            assert_eq!(validate(&body), Ok(()), "{body}");
            assert!(embedding_request_allowed(&body));
            assert_eq!(embedding_expected(&body).unwrap().inputs, inputs, "{body}");
        }
        for (extra, encoding, dimensions) in [
            (json!({}), Encoding::Float, None),
            (json!({"encoding_format": "float"}), Encoding::Float, None),
            (json!({"encoding_format": "base64"}), Encoding::Base64, None),
            (json!({"dimensions": 256}), Encoding::Float, Some(256)),
            (json!({"user": "user-7"}), Encoding::Float, None),
        ] {
            let expected = embedding_expected(&embeddings(extra.clone())).unwrap();
            assert_eq!(
                (expected.encoding, expected.dimensions),
                (encoding, dimensions),
                "{extra}"
            );
        }
        for field in [
            "stream",
            "cache_salt",
            "messages",
            "truncate_prompt_tokens",
            "request_id",
            "embed_dtype",
            "endianness",
            "use_activation",
            "rid",
            "priority",
            "lora_path",
            "embed_overrides",
            "content",
            "embd_normalize",
            "normalize",
        ] {
            assert_eq!(
                validate(&embeddings(json!({ field: false }))),
                Err(Refusal::Field(field.into())),
                "{field}"
            );
        }
        for extra in [
            json!({"input": ""}),
            json!({"input": []}),
            json!({"input": [""]}),
            json!({"input": ["a", ""]}),
            json!({"input": [[]]}),
            json!({"input": [[1], []]}),
            json!({ "input": texts(MAX_EMBEDDING_INPUTS + 1) }),
            json!({ "input": token_arrays(MAX_EMBEDDING_INPUTS + 1) }),
            json!({"input": ["a", 1]}),
            json!({"input": [1, "a"]}),
            json!({"input": [[1], 2]}),
            json!({"input": [1, [2]]}),
            json!({"input": ["a", [1]]}),
            json!({"input": [["a"]]}),
            json!({"input": [-1]}),
            json!({"input": [[-1]]}),
            json!({ "input": [MAX_TOKEN_ID + 1] }),
            json!({ "input": [[MAX_TOKEN_ID + 1]] }),
            json!({"input": [1.5]}),
            json!({"input": null}),
            json!({"input": 5}),
            json!({"input": {"type": "image_url", "image_url": {"url": "x"}}}),
            json!({"input": [{"type": "text", "text": "a"}]}),
            json!({"encoding_format": "bytes"}),
            json!({"encoding_format": "bytes_only"}),
            json!({"encoding_format": null}),
            json!({"dimensions": 0}),
            json!({"dimensions": -1}),
            json!({"dimensions": 1.5}),
            json!({"dimensions": "256"}),
            json!({"user": 7}),
            json!({"model": ""}),
            json!({"model": 1}),
        ] {
            assert!(
                matches!(
                    validate(&embeddings(extra.clone())),
                    Err(Refusal::Malformed(_))
                ),
                "{extra}"
            );
        }
        for missing in ["model", "input"] {
            let mut body = embeddings(json!({}));
            body.as_object_mut().unwrap().remove(missing);
            assert!(
                matches!(validate(&body), Err(Refusal::Malformed(_))),
                "{missing}"
            );
        }
        assert!(validate(&json!(["a"])).is_err());
        // Each endpoint keeps its own contract.
        assert!(validate_chat_request(&embeddings(json!({}))).is_err());
        assert!(validate_decision_request(&embeddings(json!({}))).is_err());
        assert!(matches!(
            validate(&decision(json!({}))),
            Err(Refusal::Field(_))
        ));
        assert!(matches!(
            AdapterError::from(Refusal::Field("stream".into())),
            AdapterError::PolicyDenied
        ));
    }

    /// ADR 0036 §3 (owner decision 2026-10-27, question 4): llama.cpp ignores
    /// `dimensions`, so the router refuses any but the native dimension.
    // T47
    #[test]
    fn only_the_native_dimensions_pass_on_llama_cpp() {
        use super::{validate_native_dimensions, EmbeddingRequestRefusal};
        assert_eq!(
            validate_native_dimensions(&embeddings(json!({})), 2560),
            Ok(())
        );
        assert_eq!(
            validate_native_dimensions(&embeddings(json!({"dimensions": 2560})), 2560),
            Ok(())
        );
        let refusal =
            validate_native_dimensions(&embeddings(json!({"dimensions": 256})), 2560).unwrap_err();
        assert_eq!(
            refusal,
            EmbeddingRequestRefusal::Dimensions { native: 2560 }
        );
        assert!(refusal
            .to_string()
            .contains("llama.cpp returns full vectors"));
    }
}
