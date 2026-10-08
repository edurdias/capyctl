//! SPEC §13.3, ADR 0008: credentials carried inside a URL never reach a log
//! line, an error, a status or a stored document. A presigned URL authorizes
//! whoever holds it through its query string, and a URL may name a user and
//! password before its host, so both are scrubbed wherever text that might
//! hold a URL leaves the process.
//!
//! [`redact_urls`] keeps what an operator needs to diagnose a fetch (the
//! scheme, host and path) and replaces everything that can authorize one.

use std::borrow::Cow;

/// What a scrubbed part of a URL is replaced with.
pub const REDACTED: &str = "<redacted>";

/// Characters that end a URL embedded in free text.
fn ends_url(c: char) -> bool {
    c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '<' | '>' | '`')
}

fn scheme_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-')
}

/// `text` with every `scheme://` URL's credentials scrubbed: userinfo
/// (`user:pass@`, `token@`) becomes `<redacted>@`, a non-empty query
/// `?<redacted>` and a non-empty fragment `#<redacted>`. The scheme, host and
/// path are kept. Any scheme is matched, not only `http(s)`. Borrowed when
/// nothing was scrubbed; applying it twice changes nothing more.
pub fn redact_urls(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    // Bytes of `text` already copied into `out`.
    let mut copied = 0;
    let mut search = 0;
    while let Some(found) = text[search..].find("://") {
        let marker = search + found;
        let body_start = marker + 3;
        let scheme_start = bytes[..marker]
            .iter()
            .rposition(|b| !scheme_byte(*b))
            .map_or(0, |index| index + 1);
        if scheme_start == marker || !bytes[scheme_start].is_ascii_alphabetic() {
            search = body_start;
            continue;
        }
        let end = text[body_start..]
            .find(ends_url)
            .map_or(text.len(), |index| body_start + index);
        let body = &text[body_start..end];
        search = end.max(body_start);
        let authority_len = body.find(['/', '?', '#']).unwrap_or(body.len());
        let (userinfo, host) = match body[..authority_len].rfind('@') {
            Some(at) => (&body[..at], &body[at + 1..authority_len]),
            None => ("", &body[..authority_len]),
        };
        let rest = &body[authority_len..];
        let path_len = rest.find(['?', '#']).unwrap_or(rest.len());
        let (path, tail) = rest.split_at(path_len);
        let (query, fragment) = match tail.strip_prefix('?') {
            Some(after) => match after.find('#') {
                Some(hash) => (Some(&after[..hash]), Some(&after[hash + 1..])),
                None => (Some(after), None),
            },
            None => (None, tail.strip_prefix('#')),
        };
        let scrub = |part: Option<&str>| part.is_some_and(|part| !part.is_empty());
        if userinfo.is_empty() && !scrub(query) && !scrub(fragment) {
            continue;
        }
        let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
        out.push_str(&text[copied..body_start]);
        if !userinfo.is_empty() {
            out.push_str(REDACTED);
            out.push('@');
        }
        out.push_str(host);
        out.push_str(path);
        if let Some(query) = query {
            out.push('?');
            out.push_str(if query.is_empty() { "" } else { REDACTED });
        }
        if let Some(fragment) = fragment {
            out.push('#');
            out.push_str(if fragment.is_empty() { "" } else { REDACTED });
        }
        copied = end;
    }
    match out {
        None => Cow::Borrowed(text),
        Some(mut out) => {
            out.push_str(&text[copied..]);
            Cow::Owned(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T37 (SPEC §13.3): a presigned URL's query authorizes the fetch, so it never
    // surfaces; the host and path stay for diagnosis.
    #[test]
    fn a_presigned_query_is_scrubbed() {
        let text = "fetch https://bucket.example.test/w/model.tar?X-Amz-Signature=abc&X-Amz-Expires=300 failed";
        assert_eq!(
            redact_urls(text),
            "fetch https://bucket.example.test/w/model.tar?<redacted> failed"
        );
        assert_eq!(
            redact_urls("s3://bucket/key?token=x#frag"),
            "s3://bucket/key?<redacted>#<redacted>"
        );
        assert_eq!(redact_urls("gs://b/k#sig"), "gs://b/k#<redacted>");
    }

    // T37 (SPEC §13.3): credentials before the host are scrubbed, in any scheme.
    #[test]
    fn userinfo_is_scrubbed() {
        assert_eq!(
            redact_urls("http://user:pass@mirror.lan:8080/w"),
            "http://<redacted>@mirror.lan:8080/w"
        );
        assert_eq!(
            redact_urls("(https://ghp_token@host)"),
            "(https://<redacted>@host)"
        );
        assert_eq!(
            redact_urls("a 'https://u:p@h/x?y=1' and \"ftp://t@h\""),
            "a 'https://<redacted>@h/x?<redacted>' and \"ftp://<redacted>@h\""
        );
    }

    // T37: text without credentials is untouched, and scrubbing is stable.
    #[test]
    fn text_without_credentials_is_borrowed_and_redaction_is_idempotent() {
        for clean in [
            "",
            "no url here",
            "https://huggingface.co/o/n/resolve/sha/config.json",
            "://bare and 9x://not-a-scheme?q=1",
            "https://host/path? and https://host/p#",
            "é https://h/ü",
        ] {
            assert!(matches!(redact_urls(clean), Cow::Borrowed(_)), "{clean}");
        }
        // Punctuation glued to a URL is part of it, so a trailing comma goes
        // with the fragment.
        let once = redact_urls("é https://u@h/p?q=1#f, then http://h2/x?a=b").into_owned();
        assert_eq!(
            once,
            "é https://<redacted>@h/p?<redacted>#<redacted> then http://h2/x?<redacted>"
        );
        assert!(matches!(redact_urls(&once), Cow::Borrowed(_)));
    }
}
