//! SPEC §10 (issue #223): an engine's answer as the engine sent it.
//!
//! The router reads an answer on its parsed form, to validate it and to take
//! its figures, but the caller receives the engine's own bytes. Without
//! serde_json's build-wide `float_roundtrip` feature, parsing a float into
//! `serde_json::Value` is not always correctly rounded, and re-encoding writes
//! the shortest form of what was parsed with the object keys sorted: a
//! probability or a log probability came back one unit in the last place away
//! from the engine's, or spelled differently (`1e-5` for Python's `1e-05`).
//! Only the top-level members CapyCTL itself sets (the public `model`, the
//! `capyctl` figures) are written anew, in place; every other member, nested
//! objects included, goes byte for byte, as a request does
//! ([`crate::request`], issue #220).

use bytes::Bytes;
use serde::de::Error as _;
use serde_json::Value;

use crate::request::{splice, StrictValue};

/// An engine's answer: its parsed form for reading and validation, and the
/// bytes it arrived as, which are what the caller receives
/// ([`Self::encode`]).
///
/// The parsed form changes only through [`Self::set`], which also names the
/// member to write anew, so the two never disagree.
#[derive(Clone, Debug)]
pub struct EngineAnswer {
    value: Value,
    /// The engine's bytes, or `None` for an answer built in process.
    raw: Option<Bytes>,
    /// The top-level members CapyCTL set, in the order first set.
    set: Vec<String>,
}

impl EngineAnswer {
    /// Parse an engine's answer: one JSON object, with a duplicated key at
    /// any depth refused, since identity and completion evidence must have
    /// one interpretation.
    pub fn parse(raw: Bytes) -> Result<Self, serde_json::Error> {
        let StrictValue(value) = serde_json::from_slice(&raw)?;
        if !value.is_object() {
            return Err(serde_json::Error::custom("an answer is a JSON object"));
        }
        Ok(Self {
            value,
            raw: Some(raw),
            set: Vec::new(),
        })
    }

    /// The parsed answer.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The parsed answer, owned.
    pub fn into_value(self) -> Value {
        self.value
    }

    /// Set the top-level member `name`: read back from [`Self::value`], and
    /// written in place of the engine's at [`Self::encode`], or after the
    /// engine's members when it sent none. An answer that is not an object
    /// has no members and is left as it is.
    pub fn set(&mut self, name: &str, value: Value) {
        let Some(object) = self.value.as_object_mut() else {
            return;
        };
        object.insert(name.to_owned(), value);
        if !self.set.iter().any(|set| set == name) {
            self.set.push(name.to_owned());
        }
    }

    /// The body to answer with: the engine's members in their order and
    /// bytes, each member CapyCTL set written with its value in its place.
    /// Fails only for an answer that is not one JSON object with distinct
    /// keys, which [`Self::parse`] refused.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        let Some(raw) = &self.raw else {
            return serde_json::to_vec(&self.value);
        };
        let replace: Vec<(&str, &Value)> = self
            .set
            .iter()
            .filter_map(|name| Some((name.as_str(), self.value.get(name)?)))
            .collect();
        splice(raw, &replace)
    }
}

impl From<Value> for EngineAnswer {
    fn from(value: Value) -> Self {
        Self {
            value,
            raw: None,
            set: Vec::new(),
        }
    }
}

impl std::ops::Deref for EngineAnswer {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Issue #223: only the members CapyCTL sets change; the engine's floats,
    // spellings and key order are its own.
    #[test]
    fn an_answer_keeps_the_engines_bytes_but_what_capyctl_sets() {
        let engine = r#"{"model":"served","b":{"z":0.9856906946328695,"a":1e-05},"a":4.332222558366402e-308}"#;
        let mut answer = EngineAnswer::parse(Bytes::from_static(engine.as_bytes())).unwrap();
        answer.set("model", json!("public"));
        answer.set("capyctl", json!({"metrics": {}}));
        answer.set("capyctl", json!({"metrics": {"n": 1}}));
        assert_eq!(answer["model"], "public");
        assert_eq!(answer["capyctl"]["metrics"]["n"], 1);
        assert_eq!(
            String::from_utf8(answer.encode().unwrap()).unwrap(),
            r#"{"model":"public","b":{"z":0.9856906946328695,"a":1e-05},"a":4.332222558366402e-308,"capyctl":{"metrics":{"n":1}}}"#
        );
    }

    #[test]
    fn only_one_unambiguous_object_is_an_answer() {
        for refused in [
            r#"[1]"#,
            r#""answer""#,
            r#"{"a":1,"a":2}"#,
            r#"{"a":{"b":1,"b":1}}"#,
            r#"{"a":1"#,
        ] {
            assert!(
                EngineAnswer::parse(Bytes::from_static(refused.as_bytes())).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn an_answer_built_in_process_encodes_its_value() {
        let mut answer = EngineAnswer::from(json!({"model": "served"}));
        answer.set("model", json!("public"));
        assert_eq!(answer.encode().unwrap(), br#"{"model":"public"}"#);
    }
}
