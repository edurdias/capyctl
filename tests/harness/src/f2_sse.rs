//! Bounded LF/CRLF SSE subset for the fixed marker runner.
use crate::{
    f2_correctness::MarkerCase,
    f2_streamed::{StreamedError, StreamedMarker},
};

/// Supports LF/CRLF, comments and data fields, including multiline JSON data.
/// Rejects other SSE fields, lone CR and incomplete final frames. No reconnect,
/// usage-only event or generic EventSource semantics are implied. HTTP status,
/// content type, clean transport completion and route provenance remain external.
/// Bounds: 1 MiB wire bytes, 4096 lines, 64 KiB line/event; no content diagnostics.
pub struct MarkerSse {
    marker: StreamedMarker,
    line: Vec<u8>,
    event: String,
    has_data: bool,
    bytes: usize,
    lines: u16,
    failed: bool,
}
impl MarkerSse {
    pub fn new(case: MarkerCase, model: &str) -> Result<Self, StreamedError> {
        Ok(Self {
            marker: StreamedMarker::new(case, model)?,
            line: Vec::new(),
            event: String::new(),
            has_data: false,
            bytes: 0,
            lines: 0,
            failed: false,
        })
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), StreamedError> {
        if self.failed {
            return Err(StreamedError);
        }
        self.failed = true;
        if bytes.len() > 1_048_576 - self.bytes {
            return Err(StreamedError);
        }
        self.bytes += bytes.len();
        for &byte in bytes {
            if byte == b'\n' {
                if self.lines == 4096 {
                    return Err(StreamedError);
                }
                self.lines += 1;
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                let line = std::str::from_utf8(&self.line).map_err(|_| StreamedError)?;
                if line.contains('\r') {
                    return Err(StreamedError);
                }
                if line.is_empty() {
                    if self.has_data {
                        self.marker.push_data(&self.event)?;
                        self.event.clear();
                        self.has_data = false;
                    }
                } else if !line.starts_with(':') {
                    let data = line.strip_prefix("data:").ok_or(StreamedError)?;
                    let data = data.strip_prefix(' ').unwrap_or(data);
                    let separator = usize::from(self.has_data);
                    if data.len() + separator > 65_536 - self.event.len() {
                        return Err(StreamedError);
                    }
                    if self.has_data {
                        self.event.push('\n');
                    }
                    self.event.push_str(data);
                    self.has_data = true;
                }
                self.line.clear();
            } else {
                if self.line.len() == 65_536 {
                    return Err(StreamedError);
                }
                self.line.push(byte);
            }
        }
        self.failed = false;
        Ok(())
    }
    /// Call only after the HTTP body ends cleanly, never on timeout or read error.
    pub fn complete(self) -> Result<(), StreamedError> {
        if self.failed || !self.line.is_empty() || self.has_data {
            return Err(StreamedError);
        }
        self.marker.complete()
    }
}
