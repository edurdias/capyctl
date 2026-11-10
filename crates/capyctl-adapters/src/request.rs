//! SPEC §10: an inference request as the caller sent it.
//!
//! The router validates a request on its parsed form, but the engine receives
//! the caller's own bytes. `serde_json::Value` keeps object keys sorted, and
//! key order can carry meaning: SGLang's System One prompt numbers a decision's
//! questions in the order of its `questions` object, and structured output
//! follows the property order of a JSON schema (issue #220). Only the
//! top-level members CapyCTL itself sets (the served `model`, `stream`, the
//! effective `cache_salt`, `rid`) are written anew, in place; every other
//! member, nested objects included, is forwarded byte for byte.
//!
//! A request with a duplicated key, at any depth, is refused at parse: what
//! was validated and what an engine's parser reads must be one request.

use bytes::Bytes;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::fmt;

/// An inference request: its parsed form for reading and validation, and the
/// bytes it arrived as, which are what an engine receives ([`Self::encode`]).
///
/// It cannot be changed after parsing, so the two never disagree: a member
/// CapyCTL sets goes as a replacement at encoding.
#[derive(Clone, Debug)]
pub struct InferenceRequest {
    value: Value,
    /// The caller's bytes, or `None` for a request built in process.
    raw: Option<Bytes>,
}

impl InferenceRequest {
    /// Parse a caller's body, refusing a duplicated key at any depth.
    pub fn parse(raw: Bytes) -> Result<Self, serde_json::Error> {
        let StrictValue(value) = serde_json::from_slice(&raw)?;
        Ok(Self {
            value,
            raw: Some(raw),
        })
    }

    /// The parsed request.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The body to send: the request's members in their order and bytes,
    /// each member named in `replace` written with its replacement in its
    /// place, and those the request does not carry appended in order. Fails
    /// only for a body that is not one JSON object with distinct keys.
    pub fn encode(&self, replace: &[(&str, &Value)]) -> Result<Vec<u8>, serde_json::Error> {
        match &self.raw {
            Some(raw) => splice(raw, replace),
            None => splice(&serde_json::to_vec(&self.value)?, replace),
        }
    }
}

impl From<Value> for InferenceRequest {
    fn from(value: Value) -> Self {
        Self { value, raw: None }
    }
}

impl std::ops::Deref for InferenceRequest {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}

/// `raw`, a JSON object, with each top-level member named in `replace`
/// written with its replacement in place, the rest byte for byte in their
/// order, and replacements it does not carry appended in order. A duplicated
/// top-level key is refused.
pub fn splice(raw: &[u8], replace: &[(&str, &Value)]) -> Result<Vec<u8>, serde_json::Error> {
    let Members(members) = serde_json::from_slice(raw)?;
    let mut out = Vec::with_capacity(raw.len() + 64);
    out.push(b'{');
    for (index, (key, value)) in members.iter().enumerate() {
        if members[..index].iter().any(|(earlier, _)| earlier == key) {
            return Err(de::Error::custom(format_args!("duplicate key `{key}`")));
        }
        if index > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, key)?;
        out.push(b':');
        match replace.iter().find(|(name, _)| name == key) {
            Some((_, replacement)) => serde_json::to_writer(&mut out, replacement)?,
            None => out.extend_from_slice(value.get().as_bytes()),
        }
    }
    let mut first = members.is_empty();
    for (name, replacement) in replace {
        if members.iter().any(|(key, _)| key == name) {
            continue;
        }
        if !first {
            out.push(b',');
        }
        first = false;
        serde_json::to_writer(&mut out, name)?;
        out.push(b':');
        serde_json::to_writer(&mut out, replacement)?;
    }
    out.push(b'}');
    Ok(out)
}

/// A JSON object's top-level members in their order, each value as its bytes.
pub(crate) struct Members<'a>(pub(crate) Vec<(String, &'a RawValue)>);

impl<'de: 'a, 'a> Deserialize<'de> for Members<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MembersVisitor<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for MembersVisitor<'a> {
            type Value = Members<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry::<String, &'de RawValue>()? {
                    members.push(member);
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(MembersVisitor(std::marker::PhantomData))
    }
}

/// `serde_json::Value` otherwise accepts a duplicated key, last value wins.
/// Identity, terminal fields and a forwarded request must have one
/// unambiguous interpretation.
pub(crate) struct StrictValue(pub(crate) Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("unambiguous JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::from(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::from(v)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::from(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v.to_owned())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = vec![];
                while let Some(StrictValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some((key, StrictValue(value))) =
                    map.next_entry::<String, StrictValue>()?
                {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom(format_args!("duplicate key `{key}`")));
                    }
                    values.insert(key, value);
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(text: &str) -> InferenceRequest {
        InferenceRequest::parse(Bytes::copy_from_slice(text.as_bytes())).unwrap()
    }

    // T19 (issue #220): the caller's key order and bytes reach the engine at
    // every depth; members CapyCTL sets replace the caller's in place, and
    // those the caller did not send follow.
    #[test]
    fn encoding_keeps_the_callers_order_and_replaces_in_place() {
        let request = parsed(
            r#"{"state":"s","model":"public","questions":{"z":{"type":"noul"},"a":{"type":"score","criteria":["lo","hi"]}},"seed":18446744073709551616}"#,
        );
        let encoded = request
            .encode(&[("model", &json!("served")), ("stream", &json!(true))])
            .unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            r#"{"state":"s","model":"served","questions":{"z":{"type":"noul"},"a":{"type":"score","criteria":["lo","hi"]}},"seed":18446744073709551616,"stream":true}"#
        );
    }

    // T19: a request built in process has no caller bytes; it is encoded from
    // its value, replacements applied the same way.
    #[test]
    fn a_request_built_in_process_is_encoded_from_its_value() {
        let request = InferenceRequest::from(json!({"model": "public", "messages": []}));
        let encoded: Value =
            serde_json::from_slice(&request.encode(&[("model", &json!("served"))]).unwrap())
                .unwrap();
        assert_eq!(encoded, json!({"model": "served", "messages": []}));
    }

    // T19: a duplicated key, at the top or nested, is refused at parse: the
    // router never validates one reading of a request while an engine's
    // parser takes another.
    #[test]
    fn a_duplicated_key_at_any_depth_is_refused() {
        for text in [
            r#"{"model":"a","model":"b"}"#,
            r#"{"model":"a","questions":{"q":{"type":"noul","type":"choice"}}}"#,
            r#"{"model":"a","messages":[{"role":"user","role":"system"}]}"#,
        ] {
            let error =
                InferenceRequest::parse(Bytes::copy_from_slice(text.as_bytes())).expect_err(text);
            assert!(error.to_string().contains("duplicate key"), "{error}");
        }
        assert!(splice(br#"{"rid":"a","rid":"b"}"#, &[]).is_err());
    }

    #[test]
    fn only_an_object_is_spliced() {
        assert!(splice(b"[1]", &[("model", &json!("m"))]).is_err());
        assert_eq!(
            splice(b"{}", &[("model", &json!("m"))]).unwrap(),
            br#"{"model":"m"}"#
        );
    }
}
