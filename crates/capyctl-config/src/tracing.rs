//! ADR 0037 §1, §7, §8, §12: a role's `observability.tracing` settings.
//!
//! Tracing is off until a role's document names an OTLP endpoint. The
//! settings are role-local, like `engine_logs`: they never reach the document
//! deployments are resolved against (`crate::remote_resources`), and no
//! deployment or profile field changes them. Parsing here is side-effect free;
//! the collector's credentials (`headers_file`) are read by
//! [`read_headers_file`] when the role builds its exporter, so a refused file
//! refuses the start without the document check touching the filesystem.
use crate::{ConfigError, ConfigErrorCode};
use serde_json::{Map, Value};
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};

/// The block every field below lives in.
const BLOCK: &str = "observability.tracing";

/// ADR 0037 §9 (owner decision 2026-10-27, question 4): the root sampling
/// ratio when nothing states one.
pub const DEFAULT_SAMPLE_RATIO: f64 = 1.0;

/// The largest `headers_file` read. A collector's credentials are a few lines.
pub const MAX_HEADERS_FILE_BYTES: u64 = 64 * 1024;

/// ADR 0037 §12: why a standalone `host.observability.tracing` takes only
/// `engines`. Also the pointer the strict schema gives for the other fields.
pub const STANDALONE_HOST_TRACING: &str =
    "a standalone document's `host.observability.tracing` takes only `engines`: the whole \
     process (router and embedded host) exports through `server.observability.tracing` \
     (endpoint, headers_file, sample_ratio, caller_context), and the engines export to its \
     endpoint";

/// ADR 0037 §2 (owner decision 2026-10-27, question 3): what the router does
/// with a valid caller trace context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CallerContext {
    /// The caller's context is the parent of the request span.
    #[default]
    Parent,
    /// A new trace whose request span links to the caller's context.
    Link,
    /// Neither trace header is read.
    Ignore,
}

impl CallerContext {
    /// The document spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Link => "link",
            Self::Ignore => "ignore",
        }
    }
}

/// ADR 0037 §8: where a role exports its spans, and the file holding the
/// collector's headers. The endpoint is shown by status and `config show`;
/// the headers never are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracingExport {
    /// An `http://` or `https://` URL naming a host, without credentials.
    pub endpoint: String,
    /// `headers_file`, an absolute path, read by [`read_headers_file`].
    pub headers_file: Option<PathBuf>,
}

impl TracingExport {
    /// ADR 0037 §7 (owner decision 2026-10-27, question 8): whether the
    /// endpoint's host is `localhost`, in `127.0.0.0/8` or `::1`.
    pub fn on_loopback(&self) -> bool {
        endpoint_host(&self.endpoint).is_some_and(|host| {
            host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        })
    }

    /// ADR 0037 §8: an `http://` endpoint off loopback, which the role warns
    /// about at start, like `authentication: none` on a network bind.
    pub fn plain_http_off_loopback(&self) -> bool {
        self.endpoint.starts_with("http://") && !self.on_loopback()
    }

    /// The collector's headers: [`read_headers_file`] of `headers_file`, or
    /// none when no file is named.
    pub fn headers(&self) -> Result<Vec<(String, String)>, ConfigError> {
        self.headers_file
            .as_deref()
            .map_or(Ok(Vec::new()), read_headers_file)
    }
}

/// ADR 0037 §12: a server's tracing (a standalone document's
/// `server.observability.tracing`, which covers the whole process).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerTracing {
    /// `None` is off (ADR 0037 §1): no header is read and nothing is exported.
    pub export: Option<TracingExport>,
    /// `sample_ratio`, from 0.0 to 1.0 ([`DEFAULT_SAMPLE_RATIO`]).
    pub sample_ratio: f64,
    pub caller_context: CallerContext,
}

impl Default for ServerTracing {
    fn default() -> Self {
        Self {
            export: None,
            sample_ratio: DEFAULT_SAMPLE_RATIO,
            caller_context: CallerContext::default(),
        }
    }
}

/// ADR 0037 §7: `observability.tracing.engines` of a host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EngineTracingSetting {
    #[default]
    Off,
    On,
}

impl EngineTracingSetting {
    /// The document spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// ADR 0037 §12: a host's tracing. In standalone, `export` is the server
/// block's: one exporter covers the process, and the engines export to its
/// endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostTracing {
    /// `None`: the host records no spans.
    pub export: Option<TracingExport>,
    /// `On` only with `export` on loopback ([`TracingExport::on_loopback`]).
    pub engines: EngineTracingSetting,
}

/// ADR 0037 §12, T03: the `observability.tracing` of a server document (or of
/// a standalone document's `server:` block). Omitted, off. Refused: a field
/// other than `endpoint`, `headers_file`, `sample_ratio` and `caller_context`;
/// an endpoint that is not an `http` or `https` URL naming a host; a relative
/// `headers_file`, or one without an endpoint; a ratio that is not a number
/// from 0 to 1; a `caller_context` other than `parent`, `link` or `ignore`.
pub fn server_tracing(document: &Value) -> Result<ServerTracing, ConfigError> {
    let Some(map) = block(
        document,
        &["endpoint", "headers_file", "sample_ratio", "caller_context"],
    )?
    else {
        return Ok(ServerTracing::default());
    };
    let sample_ratio = match map.get("sample_ratio") {
        None => DEFAULT_SAMPLE_RATIO,
        Some(value) => value
            .as_f64()
            .filter(|ratio| (0.0..=1.0).contains(ratio))
            .ok_or_else(|| refuse("sample_ratio", "must be a number from 0.0 to 1.0"))?,
    };
    let caller_context = match map.get("caller_context") {
        None => CallerContext::default(),
        Some(value) => match value.as_str() {
            Some("parent") => CallerContext::Parent,
            Some("link") => CallerContext::Link,
            Some("ignore") => CallerContext::Ignore,
            _ => {
                return Err(refuse(
                    "caller_context",
                    "must be `parent`, `link` or `ignore`",
                ))
            }
        },
    };
    Ok(ServerTracing {
        export: export(map)?,
        sample_ratio,
        caller_context,
    })
}

/// ADR 0037 §7, §12, T03: the `observability.tracing` of a host document.
/// Omitted, off. Refused as [`server_tracing`] refuses its shared fields, a
/// field other than `endpoint`, `headers_file` and `engines`, an `engines`
/// other than `off` or `on`, and `engines: on` without an endpoint on
/// loopback (owner decision 2026-10-27, question 8).
pub fn host_tracing(document: &Value) -> Result<HostTracing, ConfigError> {
    let Some(map) = block(document, &["endpoint", "headers_file", "engines"])? else {
        return Ok(HostTracing::default());
    };
    let export = export(map)?;
    let engines = engines(map, export.as_ref(), "observability.tracing.endpoint")?;
    Ok(HostTracing { export, engines })
}

/// ADR 0037 §12, T03: a standalone document's tracing.
/// `server.observability.tracing` is read as [`server_tracing`] reads a
/// server's; `host.observability.tracing` takes only `engines`
/// ([`STANDALONE_HOST_TRACING`]), and `engines: on` needs the server block's
/// endpoint on loopback. Every refusal names the field under `server.` or
/// `host.`.
pub fn standalone_tracing(document: &Value) -> Result<(ServerTracing, HostTracing), ConfigError> {
    let under = |side: &'static str| {
        move |mut error: ConfigError| {
            error.path = format!("{side}.{}", error.path);
            error
        }
    };
    let server = server_tracing(&document["server"]).map_err(under("server"))?;
    let engines = match tracing_value(&document["host"]) {
        None => EngineTracingSetting::Off,
        Some(value) => {
            let map = value.as_object().ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorCode::UnsupportedCombination,
                    format!("host.{BLOCK}"),
                    "must be a mapping",
                )
            })?;
            if let Some(field) = map.keys().find(|field| *field != "engines") {
                return Err(ConfigError::new(
                    ConfigErrorCode::UnknownField,
                    format!("host.{BLOCK}.{field}"),
                    STANDALONE_HOST_TRACING,
                ));
            }
            engines(
                map,
                server.export.as_ref(),
                "server.observability.tracing.endpoint",
            )
            .map_err(under("host"))?
        }
    };
    let host = HostTracing {
        export: server.export.clone(),
        engines,
    };
    Ok((server, host))
}

/// SPEC §13.3, ADR 0037 §8: the collector's headers from `path`, a regular
/// file owned by the user CapyCTL runs as with no group or other permission
/// bits (the rule of `model_sources.huggingface_token_file`). One
/// `Name: value` header per line; blank lines and lines starting with `#` are
/// ignored; a name is an HTTP token. A refusal names the path and the line,
/// never a value or the line's text, which may hold the credential.
pub fn read_headers_file(path: &Path) -> Result<Vec<(String, String)>, ConfigError> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    read_headers_file_owned_by(path, unsafe { libc::geteuid() })
}

/// [`read_headers_file`], with the owner the file must have.
fn read_headers_file_owned_by(
    path: &Path,
    owner: u32,
) -> Result<Vec<(String, String)>, ConfigError> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let shown = path.display().to_string();
    let io = |detail: String| ConfigError::new(ConfigErrorCode::Io, shown.clone(), detail);
    let malformed = |line: usize, detail: &str| {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            shown.clone(),
            format!("line {line}: {detail}"),
        )
    };
    // No symlink is followed, and a FIFO cannot block the start: the checks
    // run on the opened descriptor, so nothing changes between them and the
    // read.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) => {
                io("the tracing headers file is a symbolic link; name the file itself".into())
            }
            _ => io(format!("cannot open the tracing headers file: {error}")),
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| io(format!("cannot read the tracing headers file: {error}")))?;
    if !metadata.is_file() {
        return Err(io("the tracing headers file must be a regular file".into()));
    }
    if metadata.uid() != owner {
        return Err(io(format!(
            "the tracing headers file must be owned by uid {owner}, the user CapyCTL runs as; \
             it is owned by uid {}",
            metadata.uid()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io(format!(
            "the tracing headers file must have no group or other permissions (mode {:04o}); \
             run `chmod 600` on it",
            metadata.mode() & 0o7777
        )));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_HEADERS_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io(format!("cannot read the tracing headers file: {error}")))?;
    if bytes.len() as u64 > MAX_HEADERS_FILE_BYTES {
        return Err(io(format!(
            "the tracing headers file is larger than {} KiB",
            MAX_HEADERS_FILE_BYTES / 1024
        )));
    }
    let text = String::from_utf8(bytes).map_err(|error| {
        let line = 1 + error.as_bytes()[..error.utf8_error().valid_up_to()]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        malformed(line, "is not UTF-8 text")
    })?;
    let mut headers = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        let number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed(number, "is not a `Name: value` header"))?;
        let name = name.trim();
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(malformed(
                number,
                "the header name is not a valid HTTP token",
            ));
        }
        let value = value.trim();
        if value.is_empty() {
            return Err(malformed(number, "the header has no value"));
        }
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(malformed(
                number,
                "the header value holds a control character",
            ));
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    Ok(headers)
}

/// RFC 9110 §5.6.2: a `tchar`.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn refuse(field: &str, detail: &str) -> ConfigError {
    ConfigError::new(
        ConfigErrorCode::UnsupportedCombination,
        format!("{BLOCK}.{field}"),
        detail,
    )
}

/// The stated `observability.tracing` of `document`; `null` reads as absent.
fn tracing_value(document: &Value) -> Option<&Value> {
    document
        .get("observability")
        .and_then(|observability| observability.get("tracing"))
        .filter(|tracing| !tracing.is_null())
}

/// The `observability.tracing` mapping of `document`, every field in `known`.
fn block<'a>(
    document: &'a Value,
    known: &[&str],
) -> Result<Option<&'a Map<String, Value>>, ConfigError> {
    let Some(value) = tracing_value(document) else {
        return Ok(None);
    };
    let map = value.as_object().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            BLOCK,
            "must be a mapping",
        )
    })?;
    if let Some(field) = map.keys().find(|field| !known.contains(&field.as_str())) {
        return Err(ConfigError::new(
            ConfigErrorCode::UnknownField,
            format!("{BLOCK}.{field}"),
            format!("unknown field; `{BLOCK}` takes {}", known.join(", ")),
        ));
    }
    Ok(Some(map))
}

/// `endpoint` and `headers_file`; `None` without an endpoint.
fn export(map: &Map<String, Value>) -> Result<Option<TracingExport>, ConfigError> {
    let endpoint = match map.get("endpoint") {
        None => None,
        Some(value) => {
            // The value is never repeated: a URL may carry a credential.
            let text = value
                .as_str()
                .filter(|text| endpoint_host(text).is_some())
                .ok_or_else(|| {
                    refuse(
                        "endpoint",
                        "must be an http:// or https:// URL naming a host, without credentials \
                         (a collector's credentials go in `headers_file`)",
                    )
                })?;
            Some(text.to_owned())
        }
    };
    let headers_file = match map.get("headers_file") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or_else(|| refuse("headers_file", "must be an absolute path"))?,
        ),
    };
    match (endpoint, headers_file) {
        (None, Some(_)) => Err(refuse(
            "headers_file",
            "names a collector's headers, but `observability.tracing.endpoint` names no \
             collector; set the endpoint or remove `headers_file`",
        )),
        (None, None) => Ok(None),
        (Some(endpoint), headers_file) => Ok(Some(TracingExport {
            endpoint,
            headers_file,
        })),
    }
}

/// `engines` of `map`; `on` needs `export` on loopback. `endpoint` names the
/// field the engines export to, for the refusal.
fn engines(
    map: &Map<String, Value>,
    export: Option<&TracingExport>,
    endpoint: &str,
) -> Result<EngineTracingSetting, ConfigError> {
    let setting = match map.get("engines") {
        None => return Ok(EngineTracingSetting::Off),
        Some(value) => match value.as_str() {
            Some("off") => EngineTracingSetting::Off,
            Some("on") => EngineTracingSetting::On,
            _ => return Err(refuse("engines", "must be `off` or `on`")),
        },
    };
    // ADR 0037 §7 (owner decision 2026-10-27, question 8): engine export is
    // loopback only. Both engines' exporters run without TLS, and an engine
    // is third-party code; its spans stay on this machine.
    if setting == EngineTracingSetting::On && !export.is_some_and(TracingExport::on_loopback) {
        return Err(refuse(
            "engines",
            &format!(
                "`engines: on` needs `{endpoint}` on loopback (localhost, 127.0.0.0/8 or ::1): \
                 the engines export there without TLS, and their spans stay on this machine"
            ),
        ));
    }
    Ok(setting)
}

/// The lowercase host of an `http://` or `https://` endpoint (an IPv6
/// address without its brackets), or `None` when it is not such a URL, names
/// no host, carries credentials or has a port that is not 1 to 65535.
fn endpoint_host(url: &str) -> Option<String> {
    let authority = crate::model_source::url_origin(url)?.host;
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            host.parse::<Ipv6Addr>().ok()?;
            let port = match after {
                "" => None,
                after => Some(after.strip_prefix(':')?),
            };
            (host, port)
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority.as_str(), None),
        },
    };
    if let Some(port) = port {
        port.parse::<u16>().ok().filter(|port| *port != 0)?;
    }
    let named = !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b':'));
    named.then(|| host.to_owned())
}

#[cfg(test)]
mod tests;
