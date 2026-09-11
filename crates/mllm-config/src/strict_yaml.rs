//! Strict YAML loading for mllm configs.
//!
//! Pipeline: parse the document with `saphyr_parser`'s event stream
//! (rejecting duplicate mapping keys during the walk), then apply a
//! hand-written allowlist walk per [`ConfigKind`] (field sets from SPEC
//! §16, see [`crate::schema`]): unknown fields -> `UnknownField`,
//! absent required fields -> `MissingRequired`, `kind` mismatch against
//! the expected kind -> `SchemaVersion`, and unit-valued scalars
//! (`"64MiB"`, `"15m"`) checked against regex
//! `^(\d+(?:\.\d+)?)\s?(B|KiB|MiB|GiB|TiB|s|m|h|ms)$` -> `InvalidUnit`.
//! On success a normalized `serde_json::Value` view is returned.

use crate::error::{ConfigError, ConfigErrorCode};
use crate::schema::{ConfigKind, FieldSpec};
use saphyr_parser::{Event, Parser, ScalarStyle};
use serde_json::{Map, Number, Value};
use std::sync::OnceLock;

/// Size/duration scalar regex: `"64MiB"`, `"1.5GiB"`, `"15m"`, `"200ms"`, ...
fn unit_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^(\d+(?:\.\d+)?)\s?(B|KiB|MiB|GiB|TiB|s|m|h|ms)$").expect("unit regex")
    })
}

/// Parse `text` strictly for `kind` and return the normalized JSON view.
pub fn parse_strict(kind: ConfigKind, text: &str) -> Result<Value, ConfigError> {
    let root = build_value(text)?;
    let obj = root.as_object().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorCode::SchemaVersion,
            "",
            "top-level document must be a mapping",
        )
    })?;
    let sch = crate::schema::schema(kind);
    check_kind(obj, kind)?;
    check_required(obj, sch.required)?;
    check_object(obj, sch.fields, "")?;
    Ok(root)
}

/// Validate `text` as `expected`, discarding the normalized view.
pub fn validate(text: &str, expected: ConfigKind) -> Result<(), ConfigError> {
    parse_strict(expected, text).map(|_| ())
}

/// Build a `serde_json::Value` from the event stream, rejecting duplicate
/// mapping keys as they are inserted.
fn build_value(text: &str) -> Result<Value, ConfigError> {
    enum Frame {
        Map(Map<String, Value>),
        Seq(Vec<Value>),
    }

    struct Builder {
        stack: Vec<Frame>,
        key_when_pushed: Vec<Option<String>>,
        pending_key: Option<String>,
        path: Vec<String>,
        result: Option<Value>,
    }

    impl Builder {
        /// Insert `key`/`value` into the current parent (map or seq), or
        /// become the root document if there is no parent.
        fn attach(&mut self, key: Option<String>, value: Value) -> Result<(), ConfigError> {
            let Some(parent) = self.stack.last_mut() else {
                // No parent: either the root document, or a nested node that
                // ended with no key (complex mapping key) — distinguish by
                // whether a key was supplied.
                match key {
                    None => {
                        self.result = Some(value);
                        return Ok(());
                    }
                    Some(_) => {
                        return Err(ConfigError::new(
                            ConfigErrorCode::SchemaVersion,
                            "",
                            "complex mapping keys are not supported in strict configs",
                        ));
                    }
                }
            };
            let Some(key) = key else {
                return Err(ConfigError::new(
                    ConfigErrorCode::SchemaVersion,
                    "",
                    "complex mapping keys are not supported in strict configs",
                ));
            };
            self.path.push(key.clone());
            let full = self.path.join(".");
            self.path.pop();
            match parent {
                Frame::Map(map) => {
                    if map.contains_key(&key) {
                        return Err(ConfigError::new(
                            ConfigErrorCode::DuplicateKey,
                            full,
                            format!("duplicate key `{key}`"),
                        ));
                    }
                    map.insert(key, value);
                }
                Frame::Seq(seq) => seq.push(value),
            }
            Ok(())
        }

        fn start_node(&mut self, frame: Frame) {
            let key = self.pending_key.take();
            if let Some(k) = &key {
                self.path.push(k.clone());
            }
            self.key_when_pushed.push(key);
            self.stack.push(frame);
        }

        fn end_node(&mut self) -> Result<(), ConfigError> {
            let key = self
                .key_when_pushed
                .pop()
                .expect("container end without start");
            if key.is_some() {
                self.path.pop();
            }
            let value = match self.stack.pop().expect("container end without start") {
                Frame::Map(map) => Value::Object(map),
                Frame::Seq(seq) => Value::Array(seq),
            };
            self.attach(key, value)
        }

        fn scalar(&mut self, scalar: &str, style: ScalarStyle) -> Result<(), ConfigError> {
            if let Some(key) = self.pending_key.take() {
                return self.attach(Some(key), scalar_value(scalar, style));
            }
            match self.stack.last_mut() {
                None => self.result = Some(scalar_value(scalar, style)),
                Some(Frame::Map(_)) => self.pending_key = Some(as_key(scalar)),
                Some(Frame::Seq(seq)) => seq.push(scalar_value(scalar, style)),
            }
            Ok(())
        }
    }

    fn scan_err(e: saphyr_parser::ScanError) -> ConfigError {
        ConfigError::new(
            ConfigErrorCode::SchemaVersion,
            "",
            format!("YAML parse error: {e}"),
        )
    }

    let mut b = Builder {
        stack: Vec::new(),
        key_when_pushed: Vec::new(),
        pending_key: None,
        path: Vec::new(),
        result: None,
    };
    for ev in Parser::new_from_str(text) {
        let (event, _span) = ev.map_err(scan_err)?;
        match event {
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart(_)
            | Event::DocumentEnd => {}
            Event::Alias(_) => {
                return Err(ConfigError::new(
                    ConfigErrorCode::SchemaVersion,
                    "",
                    "anchors/aliases are not supported in strict configs",
                ));
            }
            Event::Scalar(scalar, style, _anchor_id, _tag) => b.scalar(&scalar, style)?,
            Event::MappingStart(_anchor_id, _tag) => b.start_node(Frame::Map(Map::new())),
            Event::MappingEnd => b.end_node()?,
            Event::SequenceStart(_anchor_id, _tag) => b.start_node(Frame::Seq(Vec::new())),
            Event::SequenceEnd => b.end_node()?,
        }
    }

    b.result
        .ok_or_else(|| ConfigError::new(ConfigErrorCode::SchemaVersion, "", "empty YAML document"))
}

/// Interpret a scalar as a JSON value. Plain scalars get YAML-typed
/// coercion (bool/null/int/float); quoted and block styles stay strings,
/// so `max_buffered_bytes_total: "64"` is distinguishable from `64`.
fn scalar_value(scalar: &str, style: ScalarStyle) -> Value {
    if style == ScalarStyle::Plain {
        match scalar {
            "true" | "True" | "TRUE" => return Value::Bool(true),
            "false" | "False" | "FALSE" => return Value::Bool(false),
            "null" | "Null" | "NULL" | "~" | "" => return Value::Null,
            _ => {}
        }
        if let Ok(i) = scalar.parse::<i64>() {
            return Value::Number(Number::from(i));
        }
        if let Ok(f) = scalar.parse::<f64>() {
            if f.is_finite() {
                if let Some(n) = Number::from_f64(f) {
                    return Value::Number(n);
                }
            }
        }
    }
    Value::String(scalar.to_string())
}

fn as_key(scalar: &str) -> String {
    scalar.to_string()
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// `kind` must agree with the kind we are validating as.
fn check_kind(obj: &Map<String, Value>, kind: ConfigKind) -> Result<(), ConfigError> {
    if let Some(k) = obj.get("kind") {
        let actual = k.as_str().unwrap_or_default();
        if actual != kind.as_str() {
            return Err(ConfigError::new(
                ConfigErrorCode::SchemaVersion,
                "kind",
                format!(
                    "document kind `{actual}` does not match expected `{}`",
                    kind.as_str()
                ),
            ));
        }
    }
    Ok(())
}

fn check_required(
    obj: &Map<String, Value>,
    required: &'static [&'static str],
) -> Result<(), ConfigError> {
    for name in required {
        if !obj.contains_key(*name) {
            return Err(ConfigError::new(
                ConfigErrorCode::MissingRequired,
                *name,
                format!("missing required field `{name}`"),
            ));
        }
    }
    Ok(())
}

/// Allowlist walk over one mapping level.
fn check_object(
    obj: &Map<String, Value>,
    fields: &'static [(&'static str, FieldSpec)],
    path: &str,
) -> Result<(), ConfigError> {
    for key in obj.keys() {
        if !fields.iter().any(|(name, _)| name == key) {
            return Err(ConfigError::new(
                ConfigErrorCode::UnknownField,
                join(path, key),
                format!("unknown field `{key}`"),
            ));
        }
    }
    for (name, spec) in fields {
        if let Some(value) = obj.get(*name) {
            check_value(value, spec, &join(path, name))?;
        }
    }
    Ok(())
}

fn check_value(value: &Value, spec: &FieldSpec, path: &str) -> Result<(), ConfigError> {
    match spec {
        FieldSpec::Scalar => {
            if value.is_object() || value.is_array() {
                return Err(ConfigError::new(
                    ConfigErrorCode::SchemaVersion,
                    path,
                    "expected a scalar value",
                ));
            }
            Ok(())
        }
        FieldSpec::Unit => {
            let ok = value
                .as_str()
                .map(|s| unit_regex().is_match(s))
                .unwrap_or(false);
            if !ok {
                return Err(ConfigError::new(
                    ConfigErrorCode::InvalidUnit,
                    path,
                    format!("expected size/duration like `64MiB` or `15m`, got `{value}`"),
                ));
            }
            Ok(())
        }
        FieldSpec::Struct(fields) => {
            let obj = value.as_object().ok_or_else(|| {
                ConfigError::new(ConfigErrorCode::SchemaVersion, path, "expected a mapping")
            })?;
            check_object(obj, fields, path)
        }
        FieldSpec::OpenMap => {
            if !value.is_object() {
                return Err(ConfigError::new(
                    ConfigErrorCode::SchemaVersion,
                    path,
                    "expected a mapping",
                ));
            }
            Ok(())
        }
        FieldSpec::Seq(item) => {
            let seq = value.as_array().ok_or_else(|| {
                ConfigError::new(ConfigErrorCode::SchemaVersion, path, "expected a sequence")
            })?;
            for (i, v) in seq.iter().enumerate() {
                check_value(v, item, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ConfigError, ConfigErrorCode};

    #[test]
    fn duplicate_key_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\nname: b\n";
        assert!(matches!(
            validate(y, ConfigKind::Server),
            Err(ConfigError {
                code: ConfigErrorCode::DuplicateKey,
                ..
            })
        ));
    }

    #[test]
    fn unknown_mllm_field_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\nfrobnicate: true\n";
        assert!(matches!(
            validate(y, ConfigKind::Server),
            Err(ConfigError {
                code: ConfigErrorCode::UnknownField,
                ..
            })
        ));
    }

    #[test]
    fn invalid_unit_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\nlisteners: {}\n\
                 scheduler:\n  queue:\n    max_buffered_bytes_total: \"64\"\n";
        assert!(matches!(
            validate(y, ConfigKind::Server),
            Err(ConfigError {
                code: ConfigErrorCode::InvalidUnit,
                ..
            })
        ));
    }

    #[test]
    fn missing_required_rejected() {
        let y = "kind: deployment\nname: d\n"; // schema_version + model missing
        assert!(matches!(
            validate(y, ConfigKind::Deployment),
            Err(ConfigError {
                code: ConfigErrorCode::MissingRequired,
                ..
            })
        ));
    }

    #[test]
    fn valid_server_parses_to_json_view() {
        let y = "schema_version: 1\nkind: server\nname: lab\n";
        let v = parse_strict(ConfigKind::Server, y).unwrap();
        assert_eq!(v["kind"], "server");
    }
}
