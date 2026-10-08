//! SPEC §13.3 / T21: an engine's log is redacted as it is written and read
//! back only as a bounded tail.
//!
//! Every engine's standard output and error pass through a redacting writer
//! (`capyctl_launchers::engine_log_relay`) on their way to the launch's private
//! log, so the file never holds a credential CapyCTL handed the engine. The
//! writer knows the launch's own secrets by value (the inference and admin keys,
//! an observation credential, any secret-named variable of the engine's
//! environment) and applies shape rules for the rest: URL credentials and query
//! strings ([`capyctl_domain::redact::redact_urls`]), secret-named assignments,
//! Hugging Face tokens, bearer values and long credential-shaped runs
//! ([`crate::vllm::args::redact_text`]).
//!
//! [`read_tail`] reads the last bytes of a log for the management surface and
//! redacts them again with the shape rules. A log written under
//! `--debug-engine-logs` is raw development output and is never served
//! (SPEC §13.3).

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use capyctl_domain::redact::{redact_urls, REDACTED};

/// A launch secret shorter than this is not matched by value: replacing every
/// occurrence of a short string would blank ordinary text, and CapyCTL issues
/// nothing that short. The shape rules still apply to it.
const OWNED_MIN_BYTES: usize = 8;

/// The default tail the management surface returns.
pub const TAIL_DEFAULT_BYTES: usize = 64 * 1024;

/// The most a single tail read returns, whatever the caller asks for.
pub const TAIL_MAX_BYTES: usize = 256 * 1024;

/// The redaction applied to every engine log line.
///
/// Deliberately neither `Debug` nor `Clone`: it holds the launch's secrets.
#[derive(Default)]
pub struct LogRedactor {
    /// Longest first, so a secret that contains another is replaced whole.
    owned: Vec<String>,
}

impl LogRedactor {
    /// The shape rules alone, for text whose launch secrets are unknown.
    pub fn new() -> Self {
        Self::default()
    }

    /// Redact `value` wherever it appears, whatever its shape.
    pub fn own(&mut self, value: &str) {
        if value.len() < OWNED_MIN_BYTES || self.owned.iter().any(|known| known == value) {
            return;
        }
        self.owned.push(value.to_owned());
        self.owned
            .sort_by_key(|known| std::cmp::Reverse(known.len()));
    }

    /// Own every secret-named variable of an engine's environment.
    pub fn own_environment(&mut self, env: &BTreeMap<String, String>) {
        for (name, value) in env {
            if is_secret_variable(name) {
                self.own(value);
            }
        }
    }

    /// The owned values in the form the writer process reads them: one
    /// hex-encoded value per line. Never logged or rendered.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for value in &self.owned {
            out.extend_from_slice(hex::encode(value).as_bytes());
            out.push(b'\n');
        }
        out
    }

    /// The inverse of [`LogRedactor::encode`]. A malformed line is skipped:
    /// the shape rules still apply.
    pub fn decode(bytes: &[u8]) -> Self {
        let mut redactor = Self::new();
        for line in bytes.split(|b| *b == b'\n') {
            if let Some(value) = hex::decode(line)
                .ok()
                .and_then(|raw| String::from_utf8(raw).ok())
            {
                redactor.own(&value);
            }
        }
        redactor
    }

    /// `text` with every launch secret and credential shape replaced by
    /// `<redacted>`.
    pub fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for value in &self.owned {
            if text.contains(value.as_str()) {
                text = text.replace(value.as_str(), REDACTED);
            }
        }
        let text = redact_urls(&text);
        let text = redact_assignments(&text);
        let text = redact_hf_tokens(&text);
        crate::vllm::args::redact_text(&text)
    }
}

/// An environment variable whose value is a credential by its name.
pub fn is_secret_variable(name: &str) -> bool {
    const SUFFIXES: [&str; 6] = [
        "_KEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_CREDENTIAL",
        "_CREDENTIALS",
    ];
    let name = name.to_ascii_uppercase();
    SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Names whose assigned value is a credential: `api_key='…'`,
/// `"password": "…"`, `Authorization: Basic …`. A longer identifier ending in
/// one of these after `_` or `-` (`admin_api_key`, `SGLANG_API_KEY`) counts too.
const SECRET_NAMES: [&str; 14] = [
    "api_key",
    "api-key",
    "apikey",
    "access_token",
    "auth_token",
    "hf_token",
    "session_token",
    "secret_key",
    "secret_access_key",
    "client_secret",
    "password",
    "passwd",
    "authorization",
    "credential",
];

fn identifier_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn secret_name(identifier: &str) -> bool {
    let identifier = identifier.to_ascii_lowercase();
    SECRET_NAMES.iter().any(|name| {
        identifier == *name
            || identifier
                .strip_suffix(name)
                .is_some_and(|head| head.ends_with('_') || head.ends_with('-'))
    })
}

fn ends_value(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b',' | b';' | b')' | b'}' | b']' | b'&')
}

/// The value assigned to a secret name, replaced: the name and separator stay
/// so an operator still sees which setting was present.
fn redact_assignments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !identifier_byte(bytes[i]) || (i > 0 && identifier_byte(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && identifier_byte(bytes[i]) {
            i += 1;
        }
        if !secret_name(&text[start..i]) {
            continue;
        }
        let mut j = i;
        if j < bytes.len() && matches!(bytes[j], b'"' | b'\'') {
            j += 1;
        }
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if j >= bytes.len() || !matches!(bytes[j], b'=' | b':') {
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if j < bytes.len() && matches!(bytes[j], b'"' | b'\'') {
            j += 1;
        }
        let value_start = j;
        let mut end = value_start;
        while end < bytes.len() && !ends_value(bytes[end]) {
            end += 1;
        }
        let value = &text[value_start..end];
        if value.is_empty() || matches!(value, "None" | "none" | "null" | REDACTED) {
            i = end;
            continue;
        }
        // `Authorization: Basic <credential>`: the scheme is followed by the secret.
        if matches!(
            value.to_ascii_lowercase().as_str(),
            "basic" | "bearer" | "token"
        ) && end < bytes.len()
            && bytes[end] == b' '
        {
            end += 1;
            while end < bytes.len() && !ends_value(bytes[end]) {
                end += 1;
            }
        }
        out.push_str(&text[copied..value_start]);
        out.push_str(REDACTED);
        copied = end;
        i = end;
    }
    out.push_str(&text[copied..]);
    out
}

/// A Hugging Face access token (`hf_` and at least 30 letters or digits),
/// whatever surrounds it: a model source's token is not credential-shaped
/// enough for the long-run rule.
fn redact_hf_tokens(text: &str) -> String {
    const MIN_BODY: usize = 30;
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(found) = text[i..].find("hf_") {
        let start = i + found;
        let body = start + 3;
        let mut end = body;
        while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
            end += 1;
        }
        let bounded = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        if bounded && end - body >= MIN_BODY {
            out.push_str(&text[copied..start]);
            out.push_str(REDACTED);
            copied = end;
        }
        i = end.max(body);
    }
    out.push_str(&text[copied..]);
    out
}

/// The marker beside a log written under `--debug-engine-logs`: raw
/// development output, never served by the management surface.
pub fn raw_marker(log: &Path) -> PathBuf {
    let mut marker = log.as_os_str().to_owned();
    marker.push(".raw");
    PathBuf::from(marker)
}

/// The previous file of a rotated log (`<log>.1`).
pub fn rotated(log: &Path, generation: u32) -> PathBuf {
    let mut path = log.as_os_str().to_owned();
    path.push(format!(".{generation}"));
    PathBuf::from(path)
}

/// The bounded, redacted end of an engine log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineLogTail {
    /// Whole lines only, every one redacted.
    pub text: String,
    /// Older output exists before `text`.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TailError {
    #[error("no engine log exists for this launch")]
    Missing,
    /// SPEC §13.3: raw development logs never reach a management response.
    #[error("the engine log was written under --debug-engine-logs and is not served")]
    Raw,
    #[error("the engine log could not be read: {0}")]
    Unreadable(String),
}

/// The last `max_bytes` (at most [`TAIL_MAX_BYTES`]) of the log at `path`,
/// continuing into the rotated `<log>.1` when the current file is shorter,
/// starting at a line boundary and redacted line by line. The returned text
/// never exceeds the bound, even where a redaction marker is longer than what
/// it replaced.
pub fn read_tail(path: &Path, max_bytes: usize) -> Result<EngineLogTail, TailError> {
    if raw_marker(path).exists() {
        return Err(TailError::Raw);
    }
    let limit = max_bytes.min(TAIL_MAX_BYTES);
    let (bytes, mut truncated) = tail_bytes(path, limit)?;
    let text = String::from_utf8_lossy(&bytes);
    let redactor = LogRedactor::new();
    let mut lines: std::collections::VecDeque<String> =
        text.lines().map(|line| redactor.redact(line)).collect();
    // A cut mid-line leaves a fragment a rule may not recognise; drop it.
    if truncated {
        lines.pop_front();
    }
    let mut size: usize = lines.iter().map(|line| line.len() + 1).sum();
    while size > limit {
        let Some(line) = lines.pop_front() else {
            break;
        };
        size -= line.len() + 1;
        truncated = true;
    }
    let mut text = String::with_capacity(size);
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    Ok(EngineLogTail { text, truncated })
}

/// Up to `limit` bytes from the end of the log, the rotated file first when
/// the current one is shorter; whether older bytes were left unread.
pub fn tail_bytes(path: &Path, limit: usize) -> Result<(Vec<u8>, bool), TailError> {
    let current = read_end(path, limit).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => TailError::Missing,
        _ => TailError::Unreadable(error.kind().to_string()),
    })?;
    let (mut bytes, mut truncated) = current;
    if !truncated && bytes.len() < limit {
        match read_end(&rotated(path, 1), limit - bytes.len()) {
            Ok((mut older, older_truncated)) => {
                truncated = older_truncated || rotated(path, 2).exists();
                older.extend_from_slice(&bytes);
                bytes = older;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(TailError::Unreadable(error.kind().to_string())),
        }
    }
    Ok((bytes, truncated))
}

fn read_end(path: &Path, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let limit = limit as u64;
    let truncated = len > limit;
    if truncated {
        file.seek(SeekFrom::Start(len - limit))?;
    }
    let mut buffer = Vec::new();
    file.take(limit).read_to_end(&mut buffer)?;
    Ok((buffer, truncated))
}

#[cfg(test)]
mod tests;
