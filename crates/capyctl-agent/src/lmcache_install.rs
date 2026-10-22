//! ADR 0033 §1: LMCache in a registered engine environment.
//!
//! **Registration.** `capyctl engine add` reads the one `lmcache-*.dist-info`
//! of the environment for LMCache's version and measures the `lmcache`
//! package tree into a `sha256:` digest with ADR 0008's machinery (bounded
//! walk, bytecode caches left out), recorded on the profile.
//!
//! **The launch-time check** is SPEC §8.1's probe by shape: it reads source
//! text of the registered environment within bounds and executes nothing (no
//! interpreter, no import). It passes when LMCache is 0.5.6 or a later 0.5.x
//! release registered with the profile, its server entry point and the
//! server options CapyCTL renders are declared, the connector the engine
//! needs is there, and `cache_salt` is threaded from the engine to LMCache's
//! object keys. For SGLang it verifies salt support, not file presence, and
//! never accepts the legacy SGLang connector (owner input on LMCache issue
//! 4825, 2026-10-22: the legacy path drops the salt). A different digest
//! from the registered one is drift, reported for the caller to handle under
//! `security.installation_drift` as for the engine package.
//!
//! A pass is not qualification (AGENTS.md): the server's start and the
//! engine's start with the connector are the live checks.

use crate::installation::{
    dist_info, dist_info_version, interpreter, site_packages, site_packages_holding,
    FingerprintError, InstallationMeasurer,
};
use capyctl_config::engine_policy::Engine;
use capyctl_config::lmcache::{
    engine_gate, lmcache_version_supported, LmcacheOffReason, LmcacheRegistration,
};
use serde::Serialize;
use std::io::Read;
use std::path::Path;

/// The LMCache package and its distribution name.
pub const PACKAGE: &str = "lmcache";
/// One source file the check reads; beyond it the shape is not read.
const MAX_SOURCE: u64 = 4 << 20;

/// LMCache measured in an environment (what `engine add` records).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LmcacheMeasurement {
    pub version: String,
    /// `sha256:<64 hex>` over the `lmcache` package tree (ADR 0008).
    pub digest: String,
    pub files: usize,
    pub bytes: u64,
}

impl LmcacheMeasurement {
    pub fn registration(&self) -> LmcacheRegistration {
        LmcacheRegistration::new(&self.version, &self.digest)
    }
}

/// A passing launch-time check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LmcacheInstall {
    /// The version the environment's dist-info declares now.
    pub version: String,
    /// The digest measured now.
    pub digest: String,
    /// ADR 0008: the measured digest differs from the registered one. The
    /// caller applies `security.installation_drift` (warn: LMCache is used
    /// and the drift shown; refuse: the launch is refused
    /// `installation_drift`) and journals it.
    pub drifted: bool,
}

/// A failing launch-time check: LMCache is off for the launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{reason}: {detail}")]
pub struct LmcacheCheckFailure {
    pub reason: LmcacheOffReason,
    /// What was missing, by path relative to `site-packages` (never an
    /// absolute path), or the reason's sentence.
    pub detail: String,
}

impl LmcacheCheckFailure {
    fn new(reason: LmcacheOffReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    fn check_failed(detail: impl Into<String>) -> Self {
        Self::new(LmcacheOffReason::CheckFailed, detail)
    }
}

/// ADR 0033 §1: LMCache in the environment of `executable`, read from files
/// only. `Ok(None)` when no `site-packages` of the environment holds
/// `lmcache`. An installation that is there but cannot be measured (more
/// than one `site-packages` or dist-info, no version, beyond the bounds) is
/// an error, and `engine add` records nothing for it.
pub fn measure_lmcache(
    measurer: &InstallationMeasurer,
    executable: &Path,
) -> Result<Option<LmcacheMeasurement>, FingerprintError> {
    let site = match site_packages_holding(PACKAGE, executable) {
        Ok(site) => site,
        Err(FingerprintError::NotFound) => return Ok(None),
        Err(other) => return Err(other),
    };
    let version = dist_info_version(&site, PACKAGE).ok_or(FingerprintError::NotFound)?;
    let (digest, files, bytes) = measurer.measure_tree(&site.join(PACKAGE))?;
    Ok(Some(LmcacheMeasurement {
        version,
        digest,
        files,
        bytes,
    }))
}

/// ADR 0033 §1: the launch-time check of a vLLM or SGLang profile's LMCache,
/// by shape. Reads files of the environment of `executable` (lexically, as
/// ADR 0018 §1) and executes nothing. `registered` is the profile's
/// `lmcache` registration.
///
/// Reasons, first applicable wins: `engine_unsupported` (not vLLM or
/// SGLang), `not_installed`, `check_failed` (the installation is not one
/// readable dist-info), `version_unsupported`, `not_registered` (installed
/// after `engine add`), `check_failed` (a shape element missing, all of them
/// named). Drift is reported in the pass.
pub fn check_lmcache(
    measurer: &InstallationMeasurer,
    engine: Engine,
    executable: &Path,
    registered: Option<&LmcacheRegistration>,
) -> Result<LmcacheInstall, LmcacheCheckFailure> {
    if !matches!(engine, Engine::Vllm | Engine::Sglang) {
        let off = engine_gate(engine, "");
        return Err(LmcacheCheckFailure::new(
            LmcacheOffReason::EngineUnsupported,
            off.map_or_else(
                || LmcacheOffReason::EngineUnsupported.sentence().to_owned(),
                |off| off.detail,
            ),
        ));
    }
    let site = match site_packages_holding(PACKAGE, executable) {
        Ok(site) => site,
        Err(FingerprintError::NotFound) => {
            return Err(LmcacheCheckFailure::new(
                LmcacheOffReason::NotInstalled,
                LmcacheOffReason::NotInstalled.sentence(),
            ))
        }
        Err(FingerprintError::Ambiguous) => {
            return Err(LmcacheCheckFailure::check_failed(
                "more than one site-packages of the environment holds lmcache",
            ))
        }
        Err(other) => {
            return Err(LmcacheCheckFailure::check_failed(format!(
                "the site-packages holding lmcache could not be read ({other})"
            )))
        }
    };
    if dist_info(&site, PACKAGE).is_none() {
        return Err(LmcacheCheckFailure::check_failed(
            "site-packages holds no single lmcache-*.dist-info",
        ));
    }
    let version = dist_info_version(&site, PACKAGE).ok_or_else(|| {
        LmcacheCheckFailure::check_failed("lmcache-*.dist-info/METADATA declares no Version")
    })?;
    if !lmcache_version_supported(&version) {
        return Err(LmcacheCheckFailure::new(
            LmcacheOffReason::VersionUnsupported,
            format!(
                "LMCache {version} is installed; {}",
                LmcacheOffReason::VersionUnsupported.sentence()
            ),
        ));
    }
    let Some(registered) = registered else {
        return Err(LmcacheCheckFailure::new(
            LmcacheOffReason::NotRegistered,
            LmcacheOffReason::NotRegistered.sentence(),
        ));
    };
    let engine_site = site_packages(engine, executable).map_err(|e| {
        LmcacheCheckFailure::check_failed(format!(
            "the site-packages holding {} could not be read ({e})",
            engine.name()
        ))
    })?;
    let bin = interpreter(executable)
        .and_then(|python| python.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let mut shape = Shape {
        lmcache: &site,
        engine: &engine_site,
        missing: Vec::new(),
    };
    shape.server(&bin);
    shape.salt_in_lmcache();
    match engine {
        Engine::Vllm => shape.vllm(),
        _ => shape.sglang(),
    }
    if !shape.missing.is_empty() {
        return Err(LmcacheCheckFailure::check_failed(shape.missing.join("; ")));
    }
    let (digest, _, _) = measurer.measure_tree(&site.join(PACKAGE)).map_err(|e| {
        LmcacheCheckFailure::check_failed(format!("LMCache's files could not be measured ({e})"))
    })?;
    Ok(LmcacheInstall {
        version,
        drifted: digest != registered.digest,
        digest,
    })
}

/// The shape elements found missing, each named by its file relative to
/// `site-packages`.
struct Shape<'a> {
    lmcache: &'a Path,
    engine: &'a Path,
    missing: Vec<String>,
}

/// Why a source file could not be read.
enum Unread {
    Absent,
    Other(&'static str),
}

/// One source file's text: a regular file (not followed through a link),
/// at most [`MAX_SOURCE`] bytes, UTF-8.
fn read_source(path: &Path) -> Result<String, Unread> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| Unread::Absent)?;
    if !metadata.is_file() {
        return Err(Unread::Other("is not a regular file"));
    }
    if metadata.len() > MAX_SOURCE {
        return Err(Unread::Other("is too large to read"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_SOURCE + 1).read_to_end(&mut bytes))
        .map_err(|_| Unread::Other("could not be read"))?;
    if bytes.len() as u64 > MAX_SOURCE {
        return Err(Unread::Other("is too large to read"));
    }
    String::from_utf8(bytes).map_err(|_| Unread::Other("is not UTF-8 text"))
}

impl Shape<'_> {
    fn note(&mut self, what: String) {
        self.missing.push(what);
    }

    /// `relative` under `site`, required: an absent or unreadable file is
    /// noted.
    fn required(&mut self, site: &Path, relative: &str) -> Option<String> {
        match read_source(&site.join(relative)) {
            Ok(text) => Some(text),
            Err(Unread::Absent) => {
                self.note(format!("{relative} is missing"));
                None
            }
            Err(Unread::Other(why)) => {
                self.note(format!("{relative} {why}"));
                None
            }
        }
    }

    /// `relative` under `site`, one of alternatives: only an unreadable
    /// file is noted.
    fn optional(&mut self, site: &Path, relative: &str) -> Option<String> {
        match read_source(&site.join(relative)) {
            Ok(text) => Some(text),
            Err(Unread::Absent) => None,
            Err(Unread::Other(why)) => {
                self.note(format!("{relative} {why}"));
                None
            }
        }
    }

    /// ADR 0033 §3: `lmcache server` and every option CapyCTL renders for it
    /// (`--host --port --transport --http-host --http-port --l1-size-gb
    /// --no-l1-use-lazy --eviction-policy LRU`). In 0.5.x the request, transport and HTTP
    /// options are declared in `lmcache/v1/multiprocess/config.py` and the L1
    /// size, its eager allocation (`--no-l1-use-lazy`: `--l1-use-lazy` is a
    /// boolean option defaulting on) and the eviction policy in
    /// `lmcache/v1/distributed/config.py` (`add_storage_manager_args`); the
    /// `server` subcommand composes them.
    fn server(&mut self, bin: &Path) {
        let site = self.lmcache;
        const MP_CONFIG: &str = "lmcache/v1/multiprocess/config.py";
        if let Some(text) = self.required(site, MP_CONFIG) {
            for option in [
                "--host",
                "--port",
                "--transport",
                "--http-host",
                "--http-port",
            ] {
                if !declares_option(&text, option) {
                    self.note(format!("{MP_CONFIG} declares no {option} option"));
                }
            }
        }
        const STORAGE_CONFIG: &str = "lmcache/v1/distributed/config.py";
        if let Some(text) = self.required(site, STORAGE_CONFIG) {
            for option in ["--l1-size-gb", "--eviction-policy"] {
                if !declares_option(&text, option) {
                    self.note(format!("{STORAGE_CONFIG} declares no {option} option"));
                }
            }
            if !text.contains("\"LRU\"") && !text.contains("'LRU'") {
                self.note(format!("{STORAGE_CONFIG} declares no LRU eviction policy"));
            }
            if !accepts_negated_flag(&text, "--l1-use-lazy") {
                self.note(format!(
                    "{STORAGE_CONFIG} declares no --no-l1-use-lazy option (--l1-use-lazy as BooleanOptionalAction)"
                ));
            }
        }
        self.entry_point(bin);
        const SERVER_COMMAND: &str = "lmcache/cli/commands/server.py";
        if let Some(text) = self.required(site, SERVER_COMMAND) {
            if !text.contains("\"server\"") && !text.contains("'server'") {
                self.note(format!("{SERVER_COMMAND} names no server subcommand"));
            }
            for composed in [
                "add_mp_server_args",
                "add_storage_manager_args",
                "add_http_frontend_args",
            ] {
                if !text.contains(composed) {
                    self.note(format!("{SERVER_COMMAND} does not compose {composed}"));
                }
            }
        }
    }

    /// The `lmcache` console script: declared in the dist-info's
    /// `entry_points.txt`, its module present and defining its function,
    /// and installed as `<environment>/bin/lmcache`.
    fn entry_point(&mut self, bin: &Path) {
        const ENTRY_POINTS: &str = "lmcache-*.dist-info/entry_points.txt";
        let target = dist_info(self.lmcache, PACKAGE)
            .and_then(|dist| read_source(&dist.join("entry_points.txt")).ok())
            .and_then(|text| console_script(&text, PACKAGE));
        match target {
            None => self.note(format!("{ENTRY_POINTS} declares no lmcache console script")),
            Some((module, function)) => {
                let relative = format!("{}.py", module.replace('.', "/"));
                let in_package = module == PACKAGE || module.starts_with("lmcache.");
                match in_package.then(|| self.required(self.lmcache, &relative)) {
                    None => self.note(format!(
                        "{ENTRY_POINTS} names {module}, outside the lmcache package"
                    )),
                    Some(None) => {}
                    Some(Some(text)) => {
                        if !defines_function(&text, &function) {
                            self.note(format!("{relative} defines no {function}()"));
                        }
                    }
                }
            }
        }
        if !std::fs::symlink_metadata(bin.join("lmcache")).is_ok_and(|m| m.is_file()) {
            self.note("bin/lmcache (the console script) is missing".to_owned());
        }
    }

    /// ADR 0033 §5: LMCache keys its objects by the salt: the IPC key the
    /// connectors send carries `cache_salt`, the object key declares it, and
    /// the server copies one into the other.
    fn salt_in_lmcache(&mut self) {
        let site = self.lmcache;
        const CUSTOM_TYPES: &str = "lmcache/v1/multiprocess/custom_types.py";
        if let Some(text) = self.required(site, CUSTOM_TYPES) {
            if !class_declares_field(&text, "IPCCacheServerKey", "cache_salt") {
                self.note(format!(
                    "no cache_salt support: {CUSTOM_TYPES} declares no cache_salt field on IPCCacheServerKey"
                ));
            }
        }
        const API: &str = "lmcache/v1/distributed/api.py";
        if let Some(text) = self.required(site, API) {
            if !class_declares_field(&text, "ObjectKey", "cache_salt") {
                self.note(format!(
                    "no cache_salt support: {API} declares no cache_salt field on ObjectKey"
                ));
            }
            let copies = function_body(&text, "ipc_key_to_object_keys").is_some_and(|body| {
                body.contains("ipc_key.cache_salt")
                    && call_has_argument(body, "ObjectKey", "cache_salt")
            });
            if !copies {
                self.note(format!(
                    "no cache_salt support: {API} does not copy ipc_key.cache_salt into ObjectKey in ipc_key_to_object_keys"
                ));
            }
        }
    }

    /// ADR 0033 §4: vLLM names `LMCacheMPConnector` by its module path, so
    /// its KV transfer configuration must accept `kv_connector_module_path`;
    /// the MP connector takes the request's salt.
    fn vllm(&mut self) {
        let site = self.lmcache;
        const CONNECTOR: &str = "lmcache/integration/vllm/lmcache_mp_connector.py";
        if let Some(text) = self.required(site, CONNECTOR) {
            if !defines_class(&text, "LMCacheMPConnector") {
                self.note(format!("{CONNECTOR} defines no class LMCacheMPConnector"));
            }
        }
        const METADATA: &str = "lmcache/integration/vllm/lmcache_mp_metadata.py";
        if let Some(text) = self.required(site, METADATA) {
            if !text.contains("request.cache_salt") {
                self.note(format!(
                    "no cache_salt support: {METADATA} does not take request.cache_salt"
                ));
            }
        }
        let engine = self.engine;
        const KV_TRANSFER: &str = "vllm/config/kv_transfer.py";
        if let Some(text) = self.required(engine, KV_TRANSFER) {
            if !class_declares_field(&text, "KVTransferConfig", "kv_connector_module_path") {
                self.note(format!(
                    "{KV_TRANSFER} declares no kv_connector_module_path on KVTransferConfig"
                ));
            }
        }
    }

    /// ADR 0033 §4, §5 (owner input 2026-10-22): SGLang through the unified
    /// LMCache integration only, which threads `cache_salt` end to end.
    fn sglang(&mut self) {
        let site = self.lmcache;
        const CONNECTOR: &str = "lmcache/integration/sglang/unified_lmcache_mp_connector.py";
        if let Some(text) = self.required(site, CONNECTOR) {
            if !defines_class(&text, "UnifiedLMCacheMPConnector") {
                self.note(format!(
                    "{CONNECTOR} defines no class UnifiedLMCacheMPConnector"
                ));
            }
            if !call_has_argument(&text, "IPCCacheServerKey", "cache_salt") {
                self.note(format!(
                    "no cache_salt support: {CONNECTOR} builds no IPCCacheServerKey with cache_salt="
                ));
            }
        }
        let engine = self.engine;
        const RADIX: &str = "sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py";
        const UNIFIED: &str = "lmcache.integration.sglang.unified_lmcache_mp_connector";
        if let Some(text) = self.required(engine, RADIX) {
            let modules = imported_modules(&text);
            if !modules.iter().any(|m| m == UNIFIED) {
                self.note(format!("{RADIX} does not import {UNIFIED}"));
            }
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            if !compact.contains("cache_salt=req.cache_salt")
                && !compact.contains("build_cache_salt(req.cache_salt")
            {
                self.note(format!(
                    "no cache_salt support: {RADIX} does not pass req.cache_salt to LMCache"
                ));
            }
            for legacy in [
                "lmcache.integration.sglang.sglang_adapter",
                "lmcache.integration.sglang.multi_process_adapter",
            ] {
                if modules
                    .iter()
                    .any(|m| m == legacy || m.starts_with(&format!("{legacy}.")))
                {
                    self.note(format!(
                        "no cache_salt support: {RADIX} imports the legacy {legacy}"
                    ));
                }
            }
        }
        const REGISTRY: &str = "sglang/srt/mem_cache/registry.py";
        if let Some(text) = self.required(engine, REGISTRY) {
            if !selects_unified_radix_cache(&text) {
                self.note(format!(
                    "{REGISTRY} does not select lmcache_unified_radix_cache under enable_lmcache"
                ));
            }
        }
        // SGLang 0.5.22 declares its options in arg groups; earlier
        // releases in `server_args.py`. Either location counts.
        const MEMORY: &str = "sglang/srt/arg_groups/fields/memory.py";
        const SERVER_ARGS: &str = "sglang/srt/server_args.py";
        let sources: Vec<String> = [MEMORY, SERVER_ARGS]
            .into_iter()
            .filter_map(|relative| self.optional(engine, relative))
            .collect();
        for field in ["enable_lmcache", "lmcache_config_file"] {
            if !sources.iter().any(|text| declares_field(text, field)) {
                self.note(format!(
                    "SGLang declares no {field} option ({MEMORY} or {SERVER_ARGS})"
                ));
            }
        }
    }
}

/// Whether `flag` is declared as a quoted option name.
fn declares_option(text: &str, flag: &str) -> bool {
    text.contains(&format!("\"{flag}\"")) || text.contains(&format!("'{flag}'"))
}

/// Whether `--no-<flag>` is accepted: declared outright, or `flag`'s own
/// `add_argument(...)` call uses `BooleanOptionalAction`.
fn accepts_negated_flag(text: &str, flag: &str) -> bool {
    let negated = format!("--no-{}", flag.trim_start_matches("--"));
    if declares_option(text, &negated) {
        return true;
    }
    ["\"", "'"].iter().any(|quote| {
        text.match_indices(&format!("{quote}{flag}{quote}"))
            .any(|(at, _)| {
                let rest = &text[at..];
                let call = rest.find("add_argument(").map_or(rest, |end| &rest[..end]);
                call.contains("BooleanOptionalAction")
            })
    })
}

fn is_identifier(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A top-level `class <name>(` or `class <name>:`.
fn defines_class(text: &str, name: &str) -> bool {
    class_body(text, name).is_some()
}

/// The indented body of the top-level class `name`: the lines after its
/// header up to the next top-level statement.
fn class_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    block_body(text, &format!("class {name}"))
}

/// The body of the top-level `def name(`.
fn function_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    block_body(text, &format!("def {name}"))
}

/// The text after a top-level line starting with `header` and then `(` or
/// `:`, up to the next top-level line that is not the header's own
/// continuation (`)`), a comment or blank.
fn block_body<'a>(text: &'a str, header: &str) -> Option<&'a str> {
    let mut offset = 0;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        let here = offset;
        offset += line.len();
        match start {
            None => {
                if let Some(rest) = line.strip_prefix(header) {
                    if rest.starts_with('(') || rest.starts_with(':') {
                        start = Some(offset);
                    }
                }
            }
            Some(begin) => {
                let top_level = line.starts_with(|c: char| !c.is_whitespace());
                if top_level && !line.starts_with(')') && !line.starts_with('#') {
                    return Some(&text[begin..here]);
                }
            }
        }
    }
    start.map(|begin| &text[begin..])
}

/// A field annotation `name:` in the class body (`cache_salt: str = ""`).
fn class_declares_field(text: &str, class: &str, field: &str) -> bool {
    class_body(text, class).is_some_and(|body| declares_field(body, field))
}

/// A line whose statement is the annotation `name: ...`.
fn declares_field(text: &str, field: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(field)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

/// A top-level `def name(`.
fn defines_function(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        line.strip_prefix(&format!("def {name}"))
            .is_some_and(|rest| rest.starts_with('('))
    })
}

/// Whether some call `callee(...)` passes the keyword argument `argument=`.
fn call_has_argument(text: &str, callee: &str, argument: &str) -> bool {
    let opener = format!("{callee}(");
    let keyword = format!("{argument}=");
    let mut from = 0;
    while let Some(found) = text[from..].find(&opener) {
        let at = from + found;
        from = at + opener.len();
        if text[..at].chars().next_back().is_some_and(is_identifier) {
            continue;
        }
        let mut depth = 1usize;
        let mut end = None;
        for (index, c) in text[from..].char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(from + index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return false;
        };
        let arguments: String = text[from..end]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let named = arguments.match_indices(&keyword).any(|(index, _)| {
            index == 0 || arguments[..index].ends_with(',') || arguments[..index].ends_with('(')
        });
        if named {
            return true;
        }
    }
    false
}

/// Every module an `import` statement of `text` imports, with its offset:
/// `import a.b` gives `a.b`; `from a.b import c, d` gives `a.b`, `a.b.c`
/// and `a.b.d` (a name may be a submodule). Parenthesized and
/// backslash-continued statements are joined.
fn import_statements(text: &str) -> Vec<(usize, Vec<String>)> {
    let mut found = Vec::new();
    let mut lines = text.split_inclusive('\n');
    let mut offset = 0;
    while let Some(line) = lines.next() {
        let at = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        if !trimmed.starts_with("import ") && !trimmed.starts_with("from ") {
            continue;
        }
        let mut statement = trimmed.trim_end().to_owned();
        let open = |s: &str| s.matches('(').count() > s.matches(')').count();
        while open(&statement) || statement.ends_with('\\') {
            let Some(next) = lines.next() else { break };
            offset += next.len();
            statement = statement.trim_end_matches('\\').to_owned();
            statement.push(' ');
            statement.push_str(next.trim());
        }
        let statement = statement.split('#').next().unwrap_or_default();
        let tokens: Vec<&str> = statement
            .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ','))
            .filter(|t| !t.is_empty())
            .collect();
        let mut modules = Vec::new();
        match tokens.as_slice() {
            ["from", base, "import", names @ ..] => {
                modules.push((*base).to_owned());
                let mut names = names.iter();
                while let Some(name) = names.next() {
                    if *name == "as" {
                        names.next();
                        continue;
                    }
                    modules.push(format!("{base}.{name}"));
                }
            }
            ["import", names @ ..] => {
                let mut names = names.iter();
                while let Some(name) = names.next() {
                    if *name == "as" {
                        names.next();
                        continue;
                    }
                    modules.push((*name).to_owned());
                }
            }
            _ => {}
        }
        found.push((at, modules));
    }
    found
}

fn imported_modules(text: &str) -> Vec<String> {
    import_statements(text)
        .into_iter()
        .flat_map(|(_, modules)| modules)
        .collect()
}

/// SGLang's cache registry: under its `if ... enable_lmcache` branch, the
/// first LMCache cache module it imports is the unified radix cache.
fn selects_unified_radix_cache(text: &str) -> bool {
    const STORAGE: &str = "sglang.srt.mem_cache.storage.lmcache.";
    const UNIFIED: &str = "sglang.srt.mem_cache.storage.lmcache.lmcache_unified_radix_cache";
    let mut offset = 0;
    let mut gate = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("if ") || trimmed.starts_with("elif "))
            && trimmed.contains("enable_lmcache")
        {
            gate = Some(offset);
            break;
        }
        offset += line.len();
    }
    let Some(gate) = gate else {
        return false;
    };
    import_statements(text)
        .into_iter()
        .filter(|(at, _)| *at > gate)
        .flat_map(|(_, modules)| modules)
        .find(|module| module.starts_with(STORAGE))
        .is_some_and(|module| module == UNIFIED || module.starts_with(&format!("{UNIFIED}.")))
}

/// `name = module:function` in the `[console_scripts]` section.
fn console_script(text: &str, name: &str) -> Option<(String, String)> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[console_scripts]";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != name {
            continue;
        }
        let value = value.split('[').next().unwrap_or_default().trim();
        let (module, function) = value.split_once(':')?;
        let (module, function) = (module.trim(), function.trim());
        let valid = |s: &str| {
            !s.is_empty()
                && s.split('.')
                    .all(|part| !part.is_empty() && part.chars().all(is_identifier))
        };
        return (valid(module) && valid(function) && !function.contains('.'))
            .then(|| (module.to_owned(), function.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lmcache_import_statements_cover_every_form() {
        let text = "import a.b as c, d\nfrom x.y import (\n    z,\n    w as v,\n)\nfrom p import \\\n    q\n";
        assert_eq!(
            imported_modules(text),
            ["a.b", "d", "x.y", "x.y.z", "x.y.w", "p", "p.q"]
        );
    }

    #[test]
    fn lmcache_calls_are_matched_by_keyword_not_by_value() {
        assert!(call_has_argument(
            "k = IPCCacheServerKey(\n  model_name=m,\n  cache_salt=op.cache_salt,\n)",
            "IPCCacheServerKey",
            "cache_salt"
        ));
        assert!(!call_has_argument(
            "k = IPCCacheServerKey(model_name=m, salt=op.cache_salt)",
            "IPCCacheServerKey",
            "cache_salt"
        ));
        assert!(!call_has_argument(
            "k = MyIPCCacheServerKey(cache_salt=s)",
            "IPCCacheServerKey",
            "cache_salt"
        ));
    }

    #[test]
    fn lmcache_class_bodies_end_at_the_next_top_level_statement() {
        let text = "class A:\n    x: int\n\n@dataclass\nclass B:\n    cache_salt: str = \"\"\n";
        assert!(!class_declares_field(text, "A", "cache_salt"));
        assert!(class_declares_field(text, "B", "cache_salt"));
    }

    #[test]
    fn lmcache_console_script_reads_its_section_only() {
        let text = "[gui_scripts]\nlmcache = other:main\n[console_scripts]\nlmcache = lmcache.cli.main:main\nlmcache_server = lmcache.v1.server.__main__:main\n";
        assert_eq!(
            console_script(text, "lmcache"),
            Some(("lmcache.cli.main".into(), "main".into()))
        );
        assert_eq!(console_script("[console_scripts]\n", "lmcache"), None);
    }
}
