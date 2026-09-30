//! Strict YAML loading for capyctl configs.
//!
//! Pipeline: parse the document with `saphyr_parser`'s event stream
//! (rejecting duplicate mapping keys during the walk), then apply a
//! hand-written allowlist walk per [`ConfigKind`] (field sets from SPEC
//! §16, see [`crate::schema`]): unknown fields -> `UnknownField`,
//! absent required fields -> `MissingRequired`, `kind` mismatch against
//! the expected kind -> `SchemaVersion`, and unit-valued scalars
//! (`"64MiB"`, `"15m"`) checked against regex
//! `^(\d+(?:\.\d+)?)\s?(B|KiB|MiB|GiB|TiB|s|m|h|ms)$` -> `InvalidUnit`.
//! Multi-document streams are rejected outright (single-document configs
//! only), and `schema_version` must be exactly `1` — both reported with
//! `SchemaVersion`. On success a normalized `serde_json::Value` view is
//! returned.

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

fn byte_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^(\d+(?:\.\d+)?)(B|KiB|MiB|GiB|TiB)$").expect("byte regex")
    })
}

fn duration_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^(\d+(?:\.\d+)?)(ms|s|m|h)$").expect("duration regex"))
}

/// Parse `text` strictly for `kind` and return the normalized JSON view.
pub fn parse_strict(kind: ConfigKind, text: &str) -> Result<Value, ConfigError> {
    parse_strict_value(kind, build_value(text)?)
}

/// The document tree of `text` (duplicate keys refused), before any schema
/// check, for a caller that completes it first (the CLI expands a `~/` model
/// path and pins a Hugging Face reference) and then calls
/// [`parse_strict_value`].
pub fn parse_document(text: &str) -> Result<Value, ConfigError> {
    build_value(text)
}

/// [`parse_strict`] on a tree [`parse_document`] built.
pub fn parse_strict_value(kind: ConfigKind, mut root: Value) -> Result<Value, ConfigError> {
    // Owner decision 2026-09-25 (ADR 0014 amendment): a deployment needs only
    // `name`, `engine` and `model`; the rest is completed here, once, so every
    // reader of a deployment document sees the same defaults. A full document
    // is unchanged.
    if kind == ConfigKind::Deployment {
        crate::deployment_defaults::expand(&mut root)?;
    }
    let obj = root.as_object().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorCode::SchemaVersion,
            "",
            "top-level document must be a mapping",
        )
    })?;
    let sch = crate::schema::schema(kind);
    check_kind(obj, kind)?;
    check_schema_version(obj)?;
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
pub(crate) fn build_value(text: &str) -> Result<Value, ConfigError> {
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
            if let Frame::Seq(seq) = parent {
                seq.push(value);
                return Ok(());
            }
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
                Frame::Seq(_) => unreachable!("sequence parent handled above"),
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
    let mut doc_count = 0usize;
    for ev in Parser::new_from_str(text) {
        let (event, _span) = ev.map_err(scan_err)?;
        match event {
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
            // Config files are single-document: reject any second document
            // instead of silently letting the last one win.
            Event::DocumentStart(_) => {
                doc_count += 1;
                if doc_count > 1 {
                    return Err(ConfigError::new(
                        ConfigErrorCode::SchemaVersion,
                        "",
                        format!(
                            "multi-document YAML stream rejected (document {doc_count}); \
                             configs must contain exactly one document"
                        ),
                    ));
                }
            }
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

/// A plain YAML scalar's value (`true`, `30`, `30s`), typed exactly as the
/// same text in a document is (owner decision 2026-09-25: a generic override
/// is validated like YAML).
pub(crate) fn plain_scalar(scalar: &str) -> Value {
    scalar_value(scalar, ScalarStyle::Plain)
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
        if let Ok(i) = scalar.parse::<u64>() {
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
    // Key identity limitation: saphyr does report key style (plain vs
    // quoted), but the normalized view is a `serde_json::Map<String, _>`,
    // so `1:` (plain) and `"1":` (quoted) necessarily collapse to the same
    // string key and are treated as the same key for duplicate detection.
    // Distinct-identity semantics would require carrying the scalar style
    // through `pending_key` and a custom map representation.
    scalar.to_string()
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// `schema_version` must be exactly the integer `1`. Missing values are
/// handled by the required-field check.
fn check_schema_version(obj: &Map<String, Value>) -> Result<(), ConfigError> {
    match obj.get("schema_version") {
        None => Ok(()),
        Some(Value::Number(n)) if n.as_i64() == Some(1) => Ok(()),
        Some(other) => Err(ConfigError::new(
            ConfigErrorCode::SchemaVersion,
            "schema_version",
            format!("unsupported schema_version `{other}`; only 1 is supported"),
        )),
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
        FieldSpec::Bytes | FieldSpec::Duration => {
            let regex = if matches!(spec, FieldSpec::Bytes) {
                byte_regex()
            } else {
                duration_regex()
            };
            let ok = value.as_str().is_some_and(|s| regex.is_match(s));
            if !ok {
                return Err(ConfigError::new(
                    ConfigErrorCode::InvalidUnit,
                    path,
                    format!("invalid typed quantity `{value}`"),
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
        FieldSpec::RequiredStruct(fields) => {
            let obj = value.as_object().ok_or_else(|| {
                ConfigError::new(ConfigErrorCode::SchemaVersion, path, "expected a mapping")
            })?;
            check_object(obj, fields, path)?;
            for (name, _) in *fields {
                if !obj.contains_key(*name) {
                    return Err(ConfigError::new(
                        ConfigErrorCode::MissingRequired,
                        join(path, name),
                        "required nested field is missing",
                    ));
                }
            }
            Ok(())
        }
        FieldSpec::ScalarOrStruct(fields) => {
            if let Some(obj) = value.as_object() {
                check_object(obj, fields, path)
            } else if value.is_array() {
                Err(ConfigError::new(
                    ConfigErrorCode::SchemaVersion,
                    path,
                    "expected a scalar or mapping",
                ))
            } else {
                Ok(())
            }
        }
        FieldSpec::MapOf(entry) => {
            let obj = value.as_object().ok_or_else(|| {
                ConfigError::new(ConfigErrorCode::SchemaVersion, path, "expected a mapping")
            })?;
            for (name, v) in obj {
                check_value(v, entry, &join(path, name))?;
            }
            Ok(())
        }
        FieldSpec::Moved(pointer) => Err(ConfigError::new(
            ConfigErrorCode::UnknownField,
            path,
            *pointer,
        )),
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
    fn unknown_capyctl_field_rejected() {
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

    #[test]
    fn unknown_nested_field_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\n\
                 scheduler:\n  queue:\n    frobnicate: 1\n";
        assert!(matches!(
            validate(y, ConfigKind::Server),
            Err(ConfigError {
                code: ConfigErrorCode::UnknownField,
                ..
            })
        ));
    }

    #[test]
    fn unknown_standalone_server_block_field_rejected() {
        let y = "schema_version: 1\nkind: standalone\nname: s\n\
                 server:\n  frobnicate: true\n";
        assert!(matches!(
            validate(y, ConfigKind::Standalone),
            Err(ConfigError {
                code: ConfigErrorCode::UnknownField,
                ..
            })
        ));
    }

    #[test]
    fn multi_document_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\n---\n\
                 schema_version: 1\nkind: server\nname: b\n";
        assert!(matches!(
            validate(y, ConfigKind::Server),
            Err(ConfigError {
                code: ConfigErrorCode::SchemaVersion,
                ..
            })
        ));
    }

    #[test]
    fn schema_version_two_rejected() {
        let y = "schema_version: 2\nkind: server\nname: a\n";
        assert!(matches!(
            validate(y, ConfigKind::Server),
            Err(ConfigError {
                code: ConfigErrorCode::SchemaVersion,
                ..
            })
        ));
    }

    #[test]
    fn valid_unit_accepted() {
        let y = "schema_version: 1\nkind: server\nname: a\nlisteners: {}\n\
                 scheduler:\n  queue:\n    max_buffered_bytes_total: \"64MiB\"\n";
        assert!(validate(y, ConfigKind::Server).is_ok());
    }

    #[test]
    fn event_builder_attaches_object_and_nested_array_elements() {
        assert_eq!(
            build_value("items:\n  - id: one\n  - id: two\n").unwrap(),
            serde_json::json!({"items": [{"id":"one"},{"id":"two"}]})
        );
        assert_eq!(
            build_value("items:\n  - - one\n    - two\n").unwrap(),
            serde_json::json!({"items": [["one","two"]]})
        );
    }

    #[test]
    fn event_builder_rejects_duplicate_keys_inside_array_objects() {
        assert_eq!(
            build_value("items:\n  - id: one\n    id: two\n")
                .unwrap_err()
                .code,
            ConfigErrorCode::DuplicateKey
        );
    }

    #[test]
    fn scalar_integer_domains_preserve_unsigned_values() {
        assert_eq!(
            scalar_value("9223372036854775808", ScalarStyle::Plain).as_u64(),
            Some(9_223_372_036_854_775_808)
        );
        assert_eq!(
            scalar_value("18446744073709551615", ScalarStyle::Plain).as_u64(),
            Some(u64::MAX)
        );
        assert_eq!(
            scalar_value("18446744073709551616", ScalarStyle::Plain).as_u64(),
            None
        );
        assert_eq!(scalar_value("-1", ScalarStyle::Plain).as_i64(), Some(-1));
        assert_eq!(
            scalar_value("42", ScalarStyle::DoubleQuoted),
            Value::String("42".into())
        );
        assert!(scalar_value("1.5", ScalarStyle::Plain).as_f64().is_some());
    }
}
