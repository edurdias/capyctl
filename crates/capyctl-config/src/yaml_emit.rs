//! Block-style YAML for a JSON tree, for the role documents `init` writes.
//!
//! Keys keep the tree's order. Strings are always double-quoted (JSON string
//! syntax is valid YAML), so a size such as `"15GiB"` or a path never changes
//! type. Empty maps and lists are written inline as `{}` and `[]`.

use serde_json::{Map, Value};

/// `value` as block-style YAML ending in a newline.
pub fn to_block_yaml(value: &Value) -> String {
    let mut out = String::new();
    if is_block(value) {
        emit_block(value, 0, &mut out);
    } else {
        out.push_str(&scalar(value));
        out.push('\n');
    }
    out
}

fn scalar(value: &Value) -> String {
    match value {
        Value::Object(_) => "{}".to_owned(),
        Value::Array(_) => "[]".to_owned(),
        other => other.to_string(),
    }
}

fn key(name: &str) -> String {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !matches!(
            name,
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
        );
    if plain {
        name.to_owned()
    } else {
        Value::String(name.to_owned()).to_string()
    }
}

fn is_block(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => false,
    }
}

fn emit_block(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::Object(map) => emit_map(map, indent, out),
        Value::Array(items) => emit_list(items, indent, out),
        _ => {}
    }
}

fn emit_map(map: &Map<String, Value>, indent: usize, out: &mut String) {
    for (name, value) in map {
        out.push_str(&" ".repeat(indent));
        emit_entry(name, value, indent, out);
    }
}

/// One `name: value` entry, the cursor already at its first column.
fn emit_entry(name: &str, value: &Value, indent: usize, out: &mut String) {
    out.push_str(&key(name));
    out.push(':');
    if is_block(value) {
        out.push('\n');
        emit_block(value, indent + 2, out);
    } else {
        out.push(' ');
        out.push_str(&scalar(value));
        out.push('\n');
    }
}

fn emit_list(items: &[Value], indent: usize, out: &mut String) {
    for item in items {
        out.push_str(&" ".repeat(indent));
        match item {
            Value::Object(map) if !map.is_empty() => {
                out.push_str("- ");
                for (i, (name, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&" ".repeat(indent + 2));
                    }
                    emit_entry(name, value, indent + 2, out);
                }
            }
            Value::Array(inner) if !inner.is_empty() => {
                out.push_str("-\n");
                emit_list(inner, indent + 2, out);
            }
            other => {
                out.push_str("- ");
                out.push_str(&scalar(other));
                out.push('\n');
            }
        }
    }
}
