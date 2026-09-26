//! Owner decision 2026-09-26: client commands (`list`, `status`, `deploy`,
//! `start`, `stop`, ...) find their management API without `--config` on
//! every command.
//!
//! Which API a command uses, first match wins:
//!
//! 1. `--context <name>` or `--config <role document>` (both is refused);
//! 2. `MLLM_CONTEXT`;
//! 3. the current saved context (`mllm context use`);
//! 4. the role running on this machine, found under the state root from what
//!    it records there: its credentials and the management address it serves
//!    on. With both a server and a standalone role recorded, the one that
//!    answers is used; if both or neither answer, the command is refused
//!    naming both and how to pick one.
//!
//! Saved contexts live in `<config home>/mllm/contexts.yaml` (0600), which is
//! also how they are stated in YAML. Each context's admin token is stored in
//! its own owner-only file under `<config home>/mllm/contexts/`; it is read
//! from `--key-file` or `MLLM_CONTEXT_KEY` when the context is added, never
//! from a command-line value. The management API is served on loopback only
//! (on every role), so a context's address is a loopback one: a role's own,
//! or the local end of an SSH port forward to another machine.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::output::StructuredError;

/// The variable naming the saved context client commands use.
pub const CONTEXT_ENV: &str = "MLLM_CONTEXT";
/// The variable `context add` reads the admin token from when `--key-file`
/// is not given.
pub const CONTEXT_KEY_ENV: &str = "MLLM_CONTEXT_KEY";
/// How long auto-detection waits for each recorded role to answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// The management address a role serves on when nothing else names one.
const DEFAULT_MANAGEMENT: &str = "127.0.0.1:7443";
/// A token longer than this is not an mllm admin token.
const MAX_TOKEN: usize = 4096;

static FLAG: OnceLock<Option<String>> = OnceLock::new();

/// Record this run's `--context` flag. Called once, before any command runs.
pub fn set_flag(context: Option<String>) {
    let _ = FLAG.set(context);
}

fn flag() -> Option<String> {
    FLAG.get().cloned().flatten()
}

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> StructuredError {
    error("invalid_config", message)
}

/// Where a target came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--config <role document>`.
    Config,
    /// `--context <name>`.
    Flag,
    /// `MLLM_CONTEXT`.
    Environment,
    /// The current saved context.
    Current,
    /// The role running on this machine.
    Detected,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Config => "config",
            Source::Flag => "flag",
            Source::Environment => "env",
            Source::Current => "current",
            Source::Detected => "detected",
        }
    }
}

/// The management API a client command uses.
#[derive(Debug, Clone)]
pub struct Target {
    /// `http://<address>/management/v1`.
    pub endpoint: String,
    pub address: SocketAddr,
    pub token: String,
    /// Where the command's request journal lives.
    pub journal_root: PathBuf,
    /// `server`, `standalone` or `context`.
    pub role: &'static str,
    /// The saved context's name, for a context.
    pub name: Option<String>,
    pub source: Source,
}

fn endpoint(address: SocketAddr) -> String {
    format!("http://{address}/management/v1")
}

/// The management API for this run: see the module documentation.
pub fn resolve(state_root: &Path, config: Option<&Path>) -> Result<Target, StructuredError> {
    resolve_with(
        state_root,
        config,
        flag().as_deref(),
        std::env::var(CONTEXT_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .as_deref(),
    )
}

fn resolve_with(
    state_root: &Path,
    config: Option<&Path>,
    flag: Option<&str>,
    env: Option<&str>,
) -> Result<Target, StructuredError> {
    match (config, flag) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "--config and --context both name a management API; use one",
            ))
        }
        (Some(path), None) => return document_target(state_root, path),
        (None, Some(name)) => return saved(state_root, name, Source::Flag),
        (None, None) => {}
    }
    if let Some(name) = env {
        return saved(state_root, name, Source::Environment);
    }
    if let Some(name) = Contexts::load()?.current {
        return saved(state_root, &name, Source::Current);
    }
    detect(state_root)
}

/// `--config`: a server document, or a standalone one.
fn document_target(state_root: &Path, path: &Path) -> Result<Target, StructuredError> {
    let standalone = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| mllm_config::parse_document(&text).ok())
        .is_some_and(|document| document["kind"] == "standalone");
    if standalone {
        return standalone_target(state_root, Source::Config);
    }
    let server = crate::remote_roles::server_context(Some(path), state_root)?;
    let (endpoint, token) = crate::remote_roles::management_context(&server)?;
    let address = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .and_then(|address| address.parse().ok())
        .unwrap_or(server.management);
    Ok(Target {
        endpoint,
        address,
        token,
        journal_root: server.state_dir,
        role: "server",
        name: None,
        source: Source::Config,
    })
}

fn standalone_credentials(state_root: &Path) -> PathBuf {
    state_root.join("identity/credentials")
}

fn server_credentials(state_root: &Path) -> PathBuf {
    state_root.join("identity/server-credentials.json")
}

/// The standalone role under `state_root`: its credentials and the address
/// it recorded (or `MLLM_MANAGEMENT_ADDR`, else its document's).
fn standalone_target(state_root: &Path, source: Source) -> Result<Target, StructuredError> {
    let credentials = std::fs::read_to_string(standalone_credentials(state_root))
        .map_err(|_| invalid("Standalone management credentials are unavailable"))?;
    let token = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .ok_or_else(|| invalid("Standalone management credential is missing"))?
        .to_owned();
    let address = crate::roles::standalone_management_address(state_root)
        .map_err(|failure| invalid(failure.to_string()))?;
    Ok(Target {
        endpoint: endpoint(address),
        address,
        token,
        journal_root: state_root.to_owned(),
        role: "standalone",
        name: None,
        source,
    })
}

/// The server whose state is `state_root`: its implicit document when there
/// is one (as before), else its credentials and recorded address.
fn server_target(state_root: &Path, source: Source) -> Result<Target, StructuredError> {
    let implicit = state_root.join("config/server.yaml");
    if std::fs::symlink_metadata(&implicit).is_ok() {
        let mut target = document_target(state_root, &implicit)?;
        target.source = source;
        return Ok(target);
    }
    let token = crate::remote_roles::server_admin_token(&state_root.join("identity"))?;
    let address = crate::roles::management_override(None)
        .map_err(|e| invalid(e.to_string()))?
        .or_else(|| crate::roles::recorded_management_address(state_root))
        .unwrap_or_else(|| DEFAULT_MANAGEMENT.parse().expect("valid default"));
    Ok(Target {
        endpoint: endpoint(address),
        address,
        token,
        journal_root: state_root.to_owned(),
        role: "server",
        name: None,
        source,
    })
}

/// The role running on this machine, from what it recorded under
/// `state_root`.
fn detect(state_root: &Path) -> Result<Target, StructuredError> {
    let standalone = std::fs::symlink_metadata(standalone_credentials(state_root)).is_ok();
    let server = std::fs::symlink_metadata(server_credentials(state_root)).is_ok();
    match (standalone, server) {
        (true, false) => standalone_target(state_root, Source::Detected),
        (false, true) => server_target(state_root, Source::Detected),
        (false, false) => Err(invalid(format!(
            "No mllm server or standalone role keeps its state under {}. Start one \
             (`mllm start standalone`), or name the management API to use: \
             --context <name> (saved with `mllm context add`) or --config <server document>",
            state_root.display()
        ))),
        (true, true) => {
            // One probe per run: a command that asks twice (its request,
            // then host names for its table) gets the same answer.
            static PROBED: std::sync::Mutex<Option<(PathBuf, Target)>> =
                std::sync::Mutex::new(None);
            if let Some((root, target)) = PROBED.lock().ok().and_then(|p| p.clone()) {
                if root == state_root {
                    return Ok(target);
                }
            }
            let candidates = [
                standalone_target(state_root, Source::Detected),
                server_target(state_root, Source::Detected),
            ];
            let answering: Vec<&Target> = candidates
                .iter()
                .filter_map(|candidate| candidate.as_ref().ok())
                .filter(|candidate| answers(candidate))
                .collect();
            if let [only] = answering[..] {
                if let Ok(mut probed) = PROBED.lock() {
                    *probed = Some((state_root.to_owned(), only.clone()));
                }
                return Ok(only.clone());
            }
            let describe = |candidate: &Result<Target, StructuredError>, role: &str| match candidate
            {
                Ok(target) => format!("{role} at {}", target.address),
                Err(_) => format!("{role} (its credentials cannot be read)"),
            };
            Err(invalid(format!(
                "Both a {} and a {} keep their state under {}, and {}; pick one: \
                 --config <server document> for the server, --config {} for standalone, \
                 or save either with `mllm context add <name> --server <address> --key-file \
                 <credentials file>` and pass --context <name> (or run `mllm context use <name>`)",
                describe(&candidates[1], "server"),
                describe(&candidates[0], "standalone role"),
                state_root.display(),
                if answering.is_empty() {
                    "neither answers"
                } else {
                    "both answer"
                },
                state_root.join("config/standalone.yaml").display(),
            )))
        }
    }
}

/// Whether `target` answers an authenticated read within the probe bound.
fn answers(target: &Target) -> bool {
    let url = format!("{}/snapshot", target.endpoint);
    let token = target.token.clone();
    // Detection runs before or inside the command's own runtime; a blocking
    // request on a plain thread serves both.
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async move {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(PROBE_TIMEOUT)
                .build()
                .ok()?;
            let response = client.get(url).bearer_auth(token).send().await.ok()?;
            Some(response.status().is_success())
        })
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// A saved context, by name.
fn saved(state_root: &Path, name: &str, source: Source) -> Result<Target, StructuredError> {
    let contexts = Contexts::load()?;
    let entry = contexts.contexts.get(name).ok_or_else(|| {
        error(
            "not_found",
            format!("No saved context is named {name:?}; `mllm context list` shows them"),
        )
    })?;
    let token = read_key(&entry.key_file)?;
    // The command's request journal is this machine's, per context.
    let journal_root = state_root.join("contexts").join(name);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&journal_root)
        .map_err(|e| invalid(format!("{}: {e}", journal_root.display())))?;
    Ok(Target {
        endpoint: endpoint(entry.server),
        address: entry.server,
        token,
        journal_root,
        role: "context",
        name: Some(name.to_owned()),
        source,
    })
}

/// The stored token of a context: an owner-only regular file.
fn read_key(path: &Path) -> Result<String, StructuredError> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| {
        invalid(format!(
            "The context's key file {} is missing",
            path.display()
        ))
    })?;
    if !meta.is_file() || meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
        return Err(invalid(format!(
            "The context's key file {} must be a regular file owned by you with mode 0600",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| invalid(format!("Cannot read {}", path.display())))?;
    token(text.trim())
}

fn token(text: &str) -> Result<String, StructuredError> {
    if text.is_empty() || text.len() > MAX_TOKEN || !text.chars().all(|c| c.is_ascii_graphic()) {
        return Err(invalid(
            "The admin token is empty or not a single printable word",
        ));
    }
    Ok(text.to_owned())
}

/// The admin token in a key file: the token alone, a server's
/// `server-credentials.json`, or a standalone `credentials` file.
fn token_in(text: &str) -> Result<String, StructuredError> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(admin) = value["admin_token"].as_str() {
            return token(admin);
        }
    }
    if let Some(admin) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("admin_token:"))
    {
        return token(admin.trim().trim_matches('"'));
    }
    token(text.trim())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[derive(Debug, Clone)]
struct Entry {
    server: SocketAddr,
    key_file: PathBuf,
}

/// `<config home>/mllm/contexts.yaml`.
#[derive(Debug, Default)]
struct Contexts {
    current: Option<String>,
    contexts: BTreeMap<String, Entry>,
}

fn store_dir() -> Result<PathBuf, StructuredError> {
    mllm_config::registration::config_home(&|key| {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    })
    .map(|home| home.join("mllm"))
    .ok_or_else(|| invalid("Neither XDG_CONFIG_HOME nor HOME is set; saved contexts need one"))
}

fn contexts_file() -> Result<PathBuf, StructuredError> {
    Ok(store_dir()?.join("contexts.yaml"))
}

fn keys_dir() -> Result<PathBuf, StructuredError> {
    Ok(store_dir()?.join("contexts"))
}

impl Contexts {
    fn load() -> Result<Self, StructuredError> {
        let Ok(path) = contexts_file() else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(invalid(format!("{}: {e}", path.display()))),
        };
        let bad = |detail: &str| invalid(format!("{}: {detail}", path.display()));
        let document = mllm_config::parse_document(&text).map_err(|e| bad(&e.detail))?;
        let current = match &document["current"] {
            Value::Null => None,
            Value::String(name) if valid_name(name) => Some(name.clone()),
            _ => return Err(bad("`current` is not a context name")),
        };
        let mut contexts = BTreeMap::new();
        match &document["contexts"] {
            Value::Null => {}
            Value::Object(entries) => {
                for (name, entry) in entries {
                    if !valid_name(name) {
                        return Err(bad(&format!("{name:?} is not a context name")));
                    }
                    let server = entry["server"]
                        .as_str()
                        .and_then(|text| text.parse().ok())
                        .ok_or_else(|| bad(&format!("contexts.{name}.server is not an address")))?;
                    let key_file = entry["key_file"]
                        .as_str()
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute())
                        .ok_or_else(|| {
                            bad(&format!("contexts.{name}.key_file is not an absolute path"))
                        })?;
                    contexts.insert(name.clone(), Entry { server, key_file });
                }
            }
            _ => return Err(bad("`contexts` is not a mapping")),
        }
        if let Some(name) = &current {
            if !contexts.contains_key(name) {
                return Err(bad(&format!("the current context {name:?} is not defined")));
            }
        }
        Ok(Self { current, contexts })
    }

    fn save(&self) -> Result<(), StructuredError> {
        let path = contexts_file()?;
        let dir = path.parent().expect("a parent").to_owned();
        private_dir(&dir)?;
        let mut text = String::from(
            "# mllm saved management contexts (`mllm context`). Tokens are not stored\n\
             # here: each key_file is an owner-only file holding one.\n",
        );
        match &self.current {
            Some(name) => text.push_str(&format!("current: {}\n", quoted(name))),
            None => text.push_str("current: null\n"),
        }
        if self.contexts.is_empty() {
            text.push_str("contexts: {}\n");
        } else {
            text.push_str("contexts:\n");
            for (name, entry) in &self.contexts {
                text.push_str(&format!(
                    "  {}:\n    server: {}\n    key_file: {}\n",
                    quoted(name),
                    quoted(&entry.server.to_string()),
                    quoted(&entry.key_file.to_string_lossy()),
                ));
            }
        }
        write_private(&path, text.as_bytes())
    }
}

/// A YAML double-quoted scalar (JSON string syntax is valid YAML).
fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

fn private_dir(dir: &Path) -> Result<(), StructuredError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| invalid(format!("{}: {e}", dir.display())))
}

/// Write `bytes` to `path`, owner-only (0600), replacing it atomically.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), StructuredError> {
    let dir = path.parent().expect("a parent");
    let temporary = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&temporary);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        invalid(format!("{}: {e}", path.display()))
    })
}

/// `mllm context add|use|list|remove|show`.
pub fn execute(
    command: &crate::grammar::Command,
    state_root: &Path,
    config: Option<&Path>,
) -> Result<Value, StructuredError> {
    use crate::grammar::Command;
    match command {
        Command::ContextAdd {
            name,
            server,
            key_file,
        } => add(name, server, key_file.as_deref()),
        Command::ContextUse { name } => {
            let mut contexts = Contexts::load()?;
            if !contexts.contexts.contains_key(name) {
                return Err(not_found(name));
            }
            contexts.current = Some(name.clone());
            contexts.save()?;
            Ok(json!({"current": name}))
        }
        Command::ContextList => {
            let contexts = Contexts::load()?;
            let items: Vec<Value> = contexts
                .contexts
                .iter()
                .map(|(name, entry)| {
                    json!({"name": name, "server": entry.server.to_string(),
                        "current": contexts.current.as_deref() == Some(name.as_str())})
                })
                .collect();
            Ok(json!({"current": contexts.current, "contexts": items}))
        }
        Command::ContextRemove { name } => {
            let mut contexts = Contexts::load()?;
            let entry = contexts
                .contexts
                .remove(name)
                .ok_or_else(|| not_found(name))?;
            if contexts.current.as_deref() == Some(name.as_str()) {
                contexts.current = None;
            }
            contexts.save()?;
            // Only a key file mllm stored is removed.
            if entry.key_file.parent() == Some(keys_dir()?.as_path()) {
                let _ = std::fs::remove_file(&entry.key_file);
            }
            Ok(json!({"removed": name, "current": contexts.current}))
        }
        Command::ContextShow => {
            let target = resolve(state_root, config)?;
            Ok(json!({
                "source": target.source.as_str(),
                "role": target.role,
                "context": target.name,
                "server": target.address.to_string(),
                "journal": target.journal_root,
            }))
        }
        _ => Err(invalid("Unsupported context command")),
    }
}

fn not_found(name: &str) -> StructuredError {
    error(
        "not_found",
        format!("No saved context is named {name:?}; `mllm context list` shows them"),
    )
}

fn add(name: &str, server: &str, key_file: Option<&Path>) -> Result<Value, StructuredError> {
    if !valid_name(name) {
        return Err(invalid(
            "A context name is letters, digits, '-', '_' or '.', at most 64, starting with a letter or digit",
        ));
    }
    let address: SocketAddr = server.parse().map_err(|_| {
        invalid(format!(
            "--server {server:?} is not an address and port such as 127.0.0.1:7443"
        ))
    })?;
    // Every role serves its management API on loopback only; its admin
    // token never crosses a network in the clear.
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(invalid(format!(
            "--server {address} is not a loopback address: mllm serves its management API on \
             loopback only. To manage another machine, forward its management port over SSH \
             (for example `ssh -N -L 17443:127.0.0.1:7443 <machine>`) and add the forwarded \
             address (--server 127.0.0.1:17443)"
        )));
    }
    let token = match key_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| invalid(format!("--key-file {}: {e}", path.display())))?;
            token_in(&text)?
        }
        None => match std::env::var(CONTEXT_KEY_ENV)
            .ok()
            .filter(|value| !value.is_empty())
        {
            Some(text) => token_in(&text)?,
            None => {
                return Err(invalid(format!(
                    "Name the admin token with --key-file <file> or {CONTEXT_KEY_ENV}; it is never \
                     taken from the command line"
                )))
            }
        },
    };
    let mut contexts = Contexts::load()?;
    if contexts.contexts.contains_key(name) {
        return Err(invalid(format!(
            "A context named {name:?} exists; `mllm context remove {name}` first"
        )));
    }
    let keys = keys_dir()?;
    private_dir(&keys)?;
    let key = keys.join(format!("{name}.key"));
    write_private(&key, format!("{token}\n").as_bytes())?;
    contexts.contexts.insert(
        name.to_owned(),
        Entry {
            server: address,
            key_file: key.clone(),
        },
    );
    contexts.save()?;
    Ok(json!({
        "context": name,
        "server": address.to_string(),
        "key_file": key,
        "current": contexts.current.as_deref() == Some(name),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_file_may_hold_the_token_or_a_role_credentials_file() {
        assert_eq!(token_in("abc123\n").unwrap(), "abc123");
        assert_eq!(
            token_in(r#"{"version":1,"admin_token":"srv","api_key":"k"}"#).unwrap(),
            "srv"
        );
        assert_eq!(token_in("api_key: k\nadmin_token: sa\n").unwrap(), "sa");
        assert!(token_in("").is_err());
        assert!(token_in("two words").is_err());
    }

    #[test]
    fn context_names_are_plain_words() {
        for name in ["prod", "gpu-box.2", "a_b"] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "-x", "a/b", "a b", &"x".repeat(65)] {
            assert!(!valid_name(name), "{name}");
        }
    }
}
