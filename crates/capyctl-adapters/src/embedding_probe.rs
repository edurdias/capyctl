//! ADR 0036 §5: the readiness probe of an embeddings deployment, one fixed
//! embedding request sent to the launch's own loopback endpoint with its own
//! key, never through ingress or the router. SPEC §6.1: `/health` and
//! `/v1/models` alone are never readiness, and an embeddings deployment is
//! never sent a chat probe in place of this one.
use serde_json::{json, Value};

use crate::traits::AdapterError;

/// The engine route the probe and every embeddings request use.
pub const PATH: &str = "/v1/embeddings";

/// ADR 0036 §5-§6: the probe's kind and version, recorded with the wake
/// canary reference. Any change to the request is a new version, and a
/// reference recorded by another kind or version is never compared.
pub const KIND: &str = "embedding";
pub const VERSION: u32 = 1;

/// ADR 0036 §5: the fixed input of version 1.
pub const INPUT: &str = "CapyCTL readiness check: a quiet library with tall shelves of old books.";

/// ADR 0036 §5: a well-formed probe answer: one finite vector whose
/// Euclidean norm is finite and above 1e-6. Its length is the instance's
/// native dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingAnswer {
    pub vector: Vec<f32>,
}

impl EmbeddingAnswer {
    /// The native dimension of the instance that answered.
    pub fn dimension(&self) -> usize {
        self.vector.len()
    }
}

/// ADR 0036 §5: the smallest Euclidean norm a ready vector has.
pub const MIN_NORM: f64 = 1e-6;

/// The largest probe answer read. One vector of one input is small: a
/// float32 written as JSON text takes at most about 25 bytes, so this
/// bound holds a vector of some 40,000 dimensions, ten times the widest
/// model ADR 0036 qualifies (4096).
pub(crate) const MAX_BODY: usize = 1024 * 1024;

/// How the probe's transport errors are named.
const LABEL: &str = "embedding probe";

/// ADR 0036 §5: the fixed request of version 1, addressed to the engine's
/// served model name.
pub fn request(served: &str) -> Value {
    json!({
        "model": served,
        "input": INPUT,
        "encoding_format": "float",
    })
}

/// ADR 0036 §5: the answer when it is completion by §4 for one input: a
/// `list` whose `data` holds exactly one entry with `index` 0 and an
/// `embedding` of at least one finite number, its Euclidean norm finite and
/// above [`MIN_NORM`]; else an error naming what was wrong (never the
/// answer's own content).
pub fn answer(response: &Value) -> Result<EmbeddingAnswer, AdapterError> {
    let failed =
        |what: &str| AdapterError::Uncertain(format!("the embedding probe answered {what}"));
    if response.get("object").and_then(Value::as_str) != Some("list") {
        return Err(failed("no list"));
    }
    let [entry] = response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| failed("no data"))?
    else {
        return Err(failed("other than one entry"));
    };
    if entry.get("index").and_then(Value::as_u64) != Some(0) {
        return Err(failed("an entry that is not index 0"));
    }
    let values = entry
        .get("embedding")
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
        .ok_or_else(|| failed("no float vector"))?;
    let vector = values
        .iter()
        .map(|value| {
            value
                .as_f64()
                .map(|value| value as f32)
                .filter(|value| value.is_finite())
                .ok_or_else(|| failed("a value that is not a finite number"))
        })
        .collect::<Result<Vec<f32>, _>>()?;
    let norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm <= MIN_NORM {
        return Err(failed("a vector whose norm is not above 1e-6"));
    }
    Ok(EmbeddingAnswer { vector })
}

/// Send the fixed probe to `base` with `key` (none for llama.cpp, ADR 0029
/// §4), bounded by `bound`, read the answer whole within [`MAX_BODY`] and
/// judge it.
pub(crate) async fn probe(
    client: &reqwest::Client,
    base: &reqwest::Url,
    key: Option<&reqwest::header::HeaderValue>,
    served: &str,
    bound: std::time::Duration,
) -> Result<EmbeddingAnswer, AdapterError> {
    let url = base
        .join(PATH)
        .map_err(|_| AdapterError::Uncertain("embedding probe: no endpoint".into()))?;
    let response = crate::completion_probe::post_json_within(
        client,
        url,
        key,
        &request(served),
        bound,
        LABEL,
        MAX_BODY,
    )
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
) -> Result<EmbeddingAnswer, AdapterError> {
    let header = match key {
        Some(key) => Some(crate::completion_probe::bearer(key).ok_or_else(|| {
            AdapterError::Uncertain("embedding probe: the key is not a header value".into())
        })?),
        None => None,
    };
    probe(client, base, header.as_ref(), served, bound).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(data: Value) -> Value {
        json!({
            "id": "embd-1",
            "object": "list",
            "created": 1,
            "model": "/models/qwen3-embedding-4b",
            "data": data,
            "usage": {"prompt_tokens": 17, "total_tokens": 17}
        })
    }

    fn entry(index: Value, embedding: Value) -> Value {
        json!({"object": "embedding", "index": index, "embedding": embedding})
    }

    // T47 (ADR 0036 §5): version 1 is the fixed input asked as floats of
    // the served model, and nothing else.
    #[test]
    fn the_request_is_the_fixed_input_as_floats() {
        assert_eq!(
            request("served"),
            json!({"model": "served", "input": INPUT, "encoding_format": "float"})
        );
        assert_eq!((KIND, VERSION, PATH), ("embedding", 1, "/v1/embeddings"));
    }

    // T47: one finite vector of index 0 whose norm is above 1e-6 is ready
    // whether or not it is normalized; its length is the native dimension.
    #[test]
    fn a_well_formed_answer_is_ready_and_carries_its_dimension() {
        let unit = answer(&engine(json!([entry(json!(0), json!([0.6, -0.8, 0.0]))]))).unwrap();
        assert_eq!(unit.vector, vec![0.6, -0.8, 0.0]);
        assert_eq!(unit.dimension(), 3);
        let raw = answer(&engine(json!([entry(json!(0), json!([12.5, 3]))]))).unwrap();
        assert_eq!(raw.dimension(), 2);
        // llama.cpp's per-entry `encoding_format` and any other member pass.
        let mut extra = entry(json!(0), json!([1e-3]));
        extra["encoding_format"] = json!("float");
        assert!(answer(&engine(json!([extra]))).is_ok());
    }

    // T47 (ADR 0036 §5): anything incomplete or malformed is not ready.
    #[test]
    fn a_malformed_answer_is_not_ready() {
        let good = entry(json!(0), json!([0.6, 0.8]));
        let mut not_a_list = engine(json!([good.clone()]));
        not_a_list["object"] = json!("embedding");
        let mut no_object = engine(json!([good.clone()]));
        no_object.as_object_mut().unwrap().remove("object");
        for bad in [
            not_a_list,
            no_object,
            json!({"object": "list"}),
            json!({"error": {"code": 400, "message": "too long"}}),
            engine(json!([])),
            engine(json!([good.clone(), entry(json!(1), json!([0.6, 0.8]))])),
            engine(json!(good.clone())),
            engine(json!([entry(json!(1), json!([0.6, 0.8]))])),
            engine(json!([entry(json!("0"), json!([0.6, 0.8]))])),
            engine(json!([entry(json!(-1), json!([0.6, 0.8]))])),
            engine(json!([entry(json!(0), json!([]))])),
            engine(json!([entry(json!(0), json!("AACAPwAAAEA="))])),
            engine(json!([entry(json!(0), json!([0.6, "0.8"]))])),
            engine(json!([entry(json!(0), json!([0.6, null]))])),
            engine(json!([entry(json!(0), json!([0.6, [0.8]]))])),
            // Finite as JSON, beyond float32.
            engine(json!([entry(json!(0), json!([0.6, 1e39]))])),
            // A zero vector, and one whose norm is not above 1e-6.
            engine(json!([entry(json!(0), json!([0.0, 0.0]))])),
            engine(json!([entry(json!(0), json!([1e-7, 1e-7]))])),
            engine(json!([{"object": "embedding", "index": 0}])),
        ] {
            assert!(answer(&bad).is_err(), "{bad}");
        }
    }

    // T47: the norm is measured in float64, so float32 values at their
    // widest still have a finite norm, and a norm just above 1e-6 is ready.
    #[test]
    fn the_norm_is_finite_and_above_a_millionth() {
        let huge = vec![json!(f32::MAX); 4];
        assert!(answer(&engine(json!([entry(json!(0), json!(huge))]))).is_ok());
        assert!(answer(&engine(json!([entry(json!(0), json!([2e-6]))]))).is_ok());
    }
}
