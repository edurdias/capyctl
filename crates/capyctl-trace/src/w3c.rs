//! W3C Trace Context Level 1 (Recommendation of 2021-11-23): the
//! `traceparent` and `tracestate` request headers, parsed and formatted.
//!
//! ADR 0037 §2: a header CapyCTL cannot parse is ignored, never refused, and
//! the request starts a new trace. Nothing here ever fails a request.

use http::HeaderMap;

/// The `traceparent` request header.
pub const TRACEPARENT: &str = "traceparent";
/// The `tracestate` request header.
pub const TRACESTATE: &str = "tracestate";

/// W3C: vendors "SHOULD propagate at least 512 characters" of `tracestate`;
/// ADR 0037 §2 drops a longer combined value whole.
pub const TRACESTATE_MAX_BYTES: usize = 512;
/// W3C: `tracestate` holds at most 32 list members.
pub const TRACESTATE_MAX_MEMBERS: usize = 32;

/// The length of a version `00` `traceparent`.
const VERSION_00_LEN: usize = 55;

/// A span context as it crosses a process boundary: trace ID, parent (span)
/// ID and flags. 25 bytes (ADR 0037 §3: what the router keeps beside an open
/// lease for a later `capyctl.settle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanCx {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub flags: u8,
}

impl SpanCx {
    /// The W3C `sampled` flag.
    pub const SAMPLED: u8 = 0x01;

    /// Whether the sampled flag is set.
    pub fn sampled(&self) -> bool {
        self.flags & Self::SAMPLED != 0
    }

    /// The `traceparent` value naming this span as the parent, always
    /// version `00` (W3C: a participant sends the version it supports).
    pub fn traceparent(&self) -> String {
        let mut out = String::with_capacity(VERSION_00_LEN);
        out.push_str("00-");
        push_hex(&mut out, &self.trace_id);
        out.push('-');
        push_hex(&mut out, &self.span_id);
        out.push('-');
        push_hex(&mut out, &[self.flags]);
        out
    }

    /// Parse a `traceparent` value under ADR 0037 §2's rules: version `00`
    /// exactly 55 characters; a higher version other than `ff` by its first
    /// four fields when at least 55 characters long and, past 55, followed by
    /// `-`; lowercase hex only; non-zero trace and parent IDs. Anything else
    /// is `None`, which the caller treats as "no context".
    pub fn parse_traceparent(value: &str) -> Option<SpanCx> {
        let bytes = value.as_bytes();
        if bytes.len() < VERSION_00_LEN {
            return None;
        }
        let version = hex_byte(&bytes[0..2])?;
        if version == 0xff {
            return None;
        }
        if version == 0x00 && bytes.len() != VERSION_00_LEN {
            return None;
        }
        // W3C §3.2.4: a higher version may append fields, each after a `-`.
        if bytes.len() > VERSION_00_LEN && bytes[VERSION_00_LEN] != b'-' {
            return None;
        }
        if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
            return None;
        }
        let mut trace_id = [0u8; 16];
        for (i, slot) in trace_id.iter_mut().enumerate() {
            *slot = hex_byte(&bytes[3 + 2 * i..5 + 2 * i])?;
        }
        let mut span_id = [0u8; 8];
        for (i, slot) in span_id.iter_mut().enumerate() {
            *slot = hex_byte(&bytes[36 + 2 * i..38 + 2 * i])?;
        }
        let flags = hex_byte(&bytes[53..55])?;
        // W3C: an all-zero trace ID or parent ID is invalid.
        if trace_id == [0u8; 16] || span_id == [0u8; 8] {
            return None;
        }
        Some(SpanCx {
            trace_id,
            span_id,
            // Only the sampled flag is defined in Level 1; the others are
            // not propagated as set (W3C §3.2.2.5: "MUST NOT" set unknown).
            flags: flags & Self::SAMPLED,
        })
    }
}

/// A caller's (or an upstream CapyCTL role's) trace context: a valid
/// `traceparent` and, only with it, a valid `tracestate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub parent: SpanCx,
    /// The combined `tracestate`, kept only when valid (ADR 0037 §2).
    pub tracestate: Option<String>,
}

impl Incoming {
    /// Read the two headers (ADR 0037 §2):
    /// - exactly one `traceparent`, valid, or the whole context is ignored
    ///   and `tracestate` is not read (W3C: "MUST NOT attempt to parse
    ///   tracestate");
    /// - `tracestate` headers joined with commas; over 512 bytes, over 32
    ///   members or not parseable is dropped whole.
    pub fn read(headers: &HeaderMap) -> Option<Incoming> {
        let mut values = headers.get_all(TRACEPARENT).iter();
        let first = values.next()?;
        if values.next().is_some() {
            return None;
        }
        let parent = SpanCx::parse_traceparent(first.to_str().ok()?)?;
        Some(Incoming {
            parent,
            tracestate: read_tracestate(headers),
        })
    }
}

/// The combined `tracestate`, or `None` when absent or invalid.
fn read_tracestate(headers: &HeaderMap) -> Option<String> {
    let mut combined = String::new();
    for value in headers.get_all(TRACESTATE).iter() {
        let value = value.to_str().ok()?;
        if !combined.is_empty() {
            combined.push(',');
        }
        combined.push_str(value);
        if combined.len() > TRACESTATE_MAX_BYTES {
            return None;
        }
    }
    let members = tracestate_members(&combined)?;
    if members.is_empty() {
        return None;
    }
    Some(members.join(","))
}

/// The list members of a `tracestate` value, or `None` when any member is
/// invalid, a key repeats, or there are more than 32 (W3C §3.3.1).
pub fn tracestate_members(value: &str) -> Option<Vec<&str>> {
    let mut members = Vec::new();
    let mut keys = Vec::new();
    for member in value.split(',') {
        // W3C: optional whitespace around members; empty members are allowed
        // and carry nothing.
        let member = member.trim_matches(|c| c == ' ' || c == '\t');
        if member.is_empty() {
            continue;
        }
        let (key, val) = member.split_once('=')?;
        if !valid_key(key) || !valid_value(val) || keys.contains(&key) {
            return None;
        }
        keys.push(key);
        members.push(member);
        if members.len() > TRACESTATE_MAX_MEMBERS {
            return None;
        }
    }
    Some(members)
}

fn lcalpha(c: u8) -> bool {
    c.is_ascii_lowercase()
}

fn key_char(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-' | b'*' | b'/')
}

/// W3C §3.3.1.3.1: `simple-key` or `tenant-id@system-id`.
fn valid_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    match key.split_once('@') {
        None => {
            !bytes.is_empty()
                && bytes.len() <= 256
                && lcalpha(bytes[0])
                && bytes[1..].iter().all(|&c| key_char(c))
        }
        Some((tenant, system)) => {
            let (t, s) = (tenant.as_bytes(), system.as_bytes());
            !t.is_empty()
                && t.len() <= 241
                && (lcalpha(t[0]) || t[0].is_ascii_digit())
                && t[1..].iter().all(|&c| key_char(c))
                && !s.is_empty()
                && s.len() <= 14
                && lcalpha(s[0])
                && s[1..].iter().all(|&c| key_char(c))
        }
    }
}

/// W3C §3.3.1.3.2: 1 to 256 printable characters other than `,` and `=`,
/// not ending in a space.
fn valid_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 256
        && bytes
            .iter()
            .all(|&c| (0x20..=0x7e).contains(&c) && c != b',' && c != b'=')
        && bytes[bytes.len() - 1] != b' '
}

fn hex_byte(pair: &[u8]) -> Option<u8> {
    Some(hex_digit(pair[0])? << 4 | hex_digit(pair[1])?)
}

/// Lowercase hex only (W3C: "HEXDIGLC").
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xf) as usize] as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    // T48: the W3C vectors ADR 0037's Verification names.
    #[test]
    fn valid_version_00_parses_and_round_trips() {
        let cx = SpanCx::parse_traceparent(VALID).expect("valid");
        assert!(cx.sampled());
        assert_eq!(cx.traceparent(), VALID);
        let unsampled =
            SpanCx::parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
                .unwrap();
        assert!(!unsampled.sampled());
    }

    // T48
    #[test]
    fn invalid_traceparents_are_ignored() {
        for bad in [
            // uppercase hex
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01",
            // all-zero trace ID
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            // all-zero parent ID
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            // version ff
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // short higher version
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0",
            // version 00 with extra fields
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-ab",
            // higher version, extra data not after a dash
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01ab",
            // wrong separators
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "",
            "garbage",
        ] {
            assert_eq!(SpanCx::parse_traceparent(bad), None, "{bad}");
        }
    }

    // T48: a higher version with extra fields is parsed by its first four.
    #[test]
    fn higher_version_with_extra_fields_is_accepted() {
        let cx = SpanCx::parse_traceparent(
            "cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-what-the-future-holds",
        )
        .expect("higher version");
        assert_eq!(cx.traceparent(), VALID);
        let exact =
            SpanCx::parse_traceparent("cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        assert!(exact.is_some());
    }

    // T48: two `traceparent` headers: ignored, and `tracestate` dropped.
    #[test]
    fn repeated_traceparent_is_ignored() {
        let map = headers(&[
            (TRACEPARENT, VALID),
            (TRACEPARENT, VALID),
            (TRACESTATE, "a=1"),
        ]);
        assert_eq!(Incoming::read(&map), None);
    }

    // T48: `tracestate` without a valid `traceparent` is discarded.
    #[test]
    fn tracestate_needs_a_valid_traceparent() {
        let map = headers(&[(TRACESTATE, "a=1")]);
        assert_eq!(Incoming::read(&map), None);
        let map = headers(&[
            (
                TRACEPARENT,
                "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            ),
            (TRACESTATE, "a=1"),
        ]);
        assert_eq!(Incoming::read(&map), None);
    }

    // T48: `tracestate` joined across headers.
    #[test]
    fn tracestate_is_joined_across_headers() {
        let map = headers(&[
            (TRACEPARENT, VALID),
            (TRACESTATE, "rojo=00f067aa0ba902b7"),
            (TRACESTATE, " congo=t61rcWkgMzE ,vendor@sys=x"),
        ]);
        let incoming = Incoming::read(&map).expect("context");
        assert_eq!(
            incoming.tracestate.as_deref(),
            Some("rojo=00f067aa0ba902b7,congo=t61rcWkgMzE,vendor@sys=x")
        );
    }

    // T48: dropped over 512 bytes or 32 members, or when unparseable; the
    // `traceparent` is kept.
    #[test]
    fn oversized_or_invalid_tracestate_is_dropped_whole() {
        let over_bytes = ["a", "b", "c"]
            .map(|k| format!("{k}={}", "x".repeat(200)))
            .join(",");
        let over_members = (0..33)
            .map(|i| format!("k{i}=v"))
            .collect::<Vec<_>>()
            .join(",");
        for state in [
            over_bytes.as_str(),
            over_members.as_str(),
            "UPPER=1",
            "a=1,a=2",
            "novalue",
            "a=bad,value",
        ] {
            let map = headers(&[(TRACEPARENT, VALID), (TRACESTATE, state)]);
            let incoming = Incoming::read(&map).expect("traceparent kept");
            assert_eq!(incoming.tracestate, None, "{state}");
        }
        let exactly_32 = (0..32)
            .map(|i| format!("k{i}=v"))
            .collect::<Vec<_>>()
            .join(",");
        let map = headers(&[(TRACEPARENT, VALID), (TRACESTATE, &exactly_32)]);
        assert!(Incoming::read(&map).unwrap().tracestate.is_some());
    }

    // T48: a non-ASCII header value is ignored, not an error.
    #[test]
    fn opaque_bytes_are_ignored() {
        let mut map = HeaderMap::new();
        map.insert(TRACEPARENT, HeaderValue::from_bytes(b"\xff\xfe").unwrap());
        assert_eq!(Incoming::read(&map), None);
    }
}
