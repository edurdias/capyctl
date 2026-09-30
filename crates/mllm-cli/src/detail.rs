//! ADR 0021: the text form of a single result. A summary line, a blank line,
//! then key-value rows indented two spaces with values aligned in one column,
//! then optional notes. `record` renders any JSON value in the same style and
//! is what `inspect` and commands without their own view print.

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detail {
    pub summary: String,
    pub rows: Vec<(String, String)>,
    pub notes: Vec<String>,
}

impl Detail {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: one_line(&summary.into()),
            ..Self::default()
        }
    }

    /// A row; an empty or `-` value is left out.
    pub fn row(mut self, key: &str, value: impl Into<String>) -> Self {
        let value = one_line(&value.into());
        if !value.is_empty() && value != "-" {
            self.rows.push((key.to_owned(), value));
        }
        self
    }

    pub fn row_opt(self, key: &str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.row(key, value),
            None => self,
        }
    }

    pub fn note(mut self, line: impl Into<String>) -> Self {
        self.notes.push(one_line(&line.into()));
        self
    }

    pub fn render(&self) -> String {
        let mut out = format!("{}\n", self.summary);
        if !self.rows.is_empty() {
            out.push('\n');
            let width = self
                .rows
                .iter()
                .map(|(k, _)| k.chars().count())
                .max()
                .unwrap_or(0);
            for (key, value) in &self.rows {
                let pad = width - key.chars().count();
                out.push_str(&format!("  {key}{}   {value}\n", " ".repeat(pad)));
            }
        }
        if !self.notes.is_empty() {
            out.push('\n');
            for note in &self.notes {
                out.push_str(note);
                out.push('\n');
            }
        }
        out
    }
}

/// Control characters (a multi-line engine error) become spaces.
pub fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `ready_instances` -> `Ready Instances`; an `id` segment reads `ID`.
fn label(key: &str) -> String {
    key.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| match w {
            "id" => "ID".to_owned(),
            _ => {
                let mut c = w.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(one_line(s)),
        Value::Bool(b) => Some(if *b { "yes" } else { "no" }.to_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => {
            let parts: Vec<String> = items.iter().filter_map(scalar).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

/// Every field of `value` under `summary`: scalars and scalar lists as rows
/// (aligned per level), objects as indented sections, object lists as
/// numbered blocks named after the singular of the key.
pub fn record(summary: &str, value: &Value) -> String {
    let mut out = format!("{}\n", one_line(summary));
    let mut body = String::new();
    fields(value, 1, &mut body);
    if !body.is_empty() {
        out.push('\n');
        out.push_str(&body);
    }
    out
}

fn fields(value: &Value, depth: usize, out: &mut String) {
    let Value::Object(map) = value else {
        if let Some(text) = scalar(value) {
            out.push_str(&format!("{}{text}\n", "  ".repeat(depth)));
        }
        return;
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let indent = "  ".repeat(depth);
    let width = keys
        .iter()
        .filter(|k| scalar(&map[k.as_str()]).is_some())
        .map(|k| label(k).chars().count())
        .max()
        .unwrap_or(0);
    for key in keys {
        let item = &map[key.as_str()];
        if let Some(text) = scalar(item) {
            let name = label(key);
            let pad = width - name.chars().count();
            out.push_str(&format!("{indent}{name}{}   {text}\n", " ".repeat(pad)));
        } else if item.is_object() {
            out.push_str(&format!("{indent}{}\n", label(key)));
            fields(item, depth + 1, out);
        } else if let Value::Array(items) = item {
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("{indent}{}\n", label(key)));
            let singular = label(key.strip_suffix('s').unwrap_or(key));
            for (i, entry) in items.iter().enumerate() {
                out.push_str(&format!("{indent}  {singular} {i}\n"));
                fields(entry, depth + 2, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // T02 (ADR 0021): summary, blank line, aligned two-space-indented rows,
    // then notes; empty values are left out.
    #[test]
    fn detail_aligns_rows_and_skips_empty_values() {
        let text = Detail::new("Deployed my-model: ready")
            .row("Revision", "1")
            .row("Hosts", "gpu-box")
            .row_opt("Context", None)
            .row("Operation", "")
            .note("Keep it private.")
            .render();
        assert_eq!(
            text,
            "Deployed my-model: ready\n\n  Revision   1\n  Hosts      gpu-box\n\nKeep it private.\n"
        );
    }

    // Review focus 4: a multi-line value cannot break the layout.
    #[test]
    fn control_characters_become_spaces() {
        let text = Detail::new("Failed")
            .row("Error", "line one\nline two")
            .render();
        assert_eq!(text, "Failed\n\n  Error   line one line two\n");
    }

    // T02 (ADR 0021): the generic record keeps every field: nested objects
    // become sections, object arrays numbered blocks, scalar arrays a list.
    #[test]
    fn record_renders_every_field() {
        let value = json!({
            "name": "my-model",
            "ready_instances": 1,
            "routes": ["my-model", "alias"],
            "timeouts": {"wake_ms": 105000, "provenance": {"wake": "derived"}},
            "instances": [{"index": 0, "host_id": "h1"}],
            "legacy_route": null
        });
        assert_eq!(
            record("Deployment my-model", &value),
            "Deployment my-model\n\n\
             \x20 Instances\n\
             \x20   Instance 0\n\
             \x20     Host ID   h1\n\
             \x20     Index     0\n\
             \x20 Name              my-model\n\
             \x20 Ready Instances   1\n\
             \x20 Routes            my-model, alias\n\
             \x20 Timeouts\n\
             \x20   Provenance\n\
             \x20     Wake   derived\n\
             \x20   Wake Ms   105000\n"
        );
    }
}
