//! ADR 0021: the one place role output is written. Library crates call
//! [`event`] and [`notice`] instead of printing; the CLI sets the mode at role
//! start (text on a terminal, JSON otherwise) and installs the text formatter.
//! The default is JSON, so library tests and embedded use see today's lines.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Json,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Notice,
    Warning,
}

/// Text for one structured event, or `None` to use the generic fallback. A
/// formatter that wants the fallback with its own decoration calls [`fallback`].
pub type Formatter = fn(&Value) -> Option<String>;

static MODE: AtomicU8 = AtomicU8::new(0);
static FORMATTER: OnceLock<Formatter> = OnceLock::new();

pub fn set_mode(mode: Mode) {
    MODE.store(matches!(mode, Mode::Text) as u8, Ordering::Relaxed);
}

pub fn mode() -> Mode {
    if MODE.load(Ordering::Relaxed) == 1 {
        Mode::Text
    } else {
        Mode::Json
    }
}

/// The first installed formatter wins; a second call is ignored.
pub fn set_formatter(formatter: Formatter) {
    let _ = FORMATTER.set(formatter);
}

/// Write one structured event to stderr in the current mode.
pub fn event(value: Value) {
    eprintln!("{}", render_event(&value));
}

/// Write one notice or warning to stderr in the current mode.
pub fn notice(level: Level, message: &str) {
    eprintln!("{}", render_notice(level, message));
}

pub fn render_event(value: &Value) -> String {
    render_event_in(mode(), FORMATTER.get().copied(), value)
}

pub fn render_notice(level: Level, message: &str) -> String {
    render_notice_in(mode(), level, message)
}

fn render_event_in(mode: Mode, formatter: Option<Formatter>, value: &Value) -> String {
    match mode {
        Mode::Json => value.to_string(),
        Mode::Text => formatter
            .and_then(|f| f(value))
            .unwrap_or_else(|| fallback(value)),
    }
}

fn render_notice_in(mode: Mode, level: Level, message: &str) -> String {
    let name = match level {
        Level::Notice => "notice",
        Level::Warning => "warning",
    };
    match mode {
        Mode::Json => serde_json::json!({"level": name, "message": message}).to_string(),
        Mode::Text => format!("{name}: {message}"),
    }
}

/// `<event> key=value …` over the scalar fields, sorted by key; nested
/// values are left out (they stay in JSON mode).
pub fn fallback(value: &Value) -> String {
    let name = value["event"].as_str().unwrap_or("event");
    let mut out = name.to_owned();
    if let Value::Object(fields) = value {
        let mut keys: Vec<&String> = fields.keys().filter(|k| k.as_str() != "event").collect();
        keys.sort();
        for key in keys {
            let text = match &fields[key] {
                Value::String(s) => s.replace(char::is_control, " "),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            out.push_str(&format!(" {key}={text}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{render_event_in, render_notice_in, Level, Mode};
    use serde_json::json;

    // T02 (ADR 0021): JSON mode prints the event unchanged; notices become objects.
    #[test]
    fn json_mode_keeps_events_and_wraps_notices() {
        let event = json!({"event": "switch", "phase": "Planned"});
        assert_eq!(render_event_in(Mode::Json, None, &event), event.to_string());
        assert_eq!(
            render_notice_in(Mode::Json, Level::Warning, "disk low"),
            json!({"level": "warning", "message": "disk low"}).to_string()
        );
    }

    // T02 (ADR 0021): text mode uses the formatter, else a key=value fallback,
    // never raw JSON.
    #[test]
    fn text_mode_formats_or_falls_back() {
        fn formatter(value: &serde_json::Value) -> Option<String> {
            (value["event"] == "known").then(|| "known event".to_owned())
        }
        let known = json!({"event": "known"});
        assert_eq!(
            render_event_in(Mode::Text, Some(formatter), &known),
            "known event"
        );
        let other = json!({"event": "other", "stage": "wake", "n": 2, "nested": {"a": 1}});
        let line = render_event_in(Mode::Text, Some(formatter), &other);
        assert!(line.ends_with("other n=2 stage=wake"), "{line}");
        assert!(!line.starts_with('{'), "{line}");
        assert_eq!(
            render_notice_in(Mode::Text, Level::Warning, "disk low"),
            "warning: disk low"
        );
        assert_eq!(
            render_notice_in(Mode::Text, Level::Notice, "hello"),
            "notice: hello"
        );
    }
}
