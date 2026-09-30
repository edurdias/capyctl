//! ADR 0018 §3: the role's local control channel, `<state_dir>/control.sock`.
//! Mode 0600 inside the role's 0700 state directory; a connection is served
//! only when `SO_PEERCRED` names the user id running capyctl. One JSON request
//! line and one JSON reply line per connection, each at most 64 KiB. It
//! carries engine add, remove and list only, and never reaches an engine.
use serde_json::{json, Value};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub const SOCKET_NAME: &str = "control.sock";
pub const MAX_LINE: usize = 64 * 1024;
/// `sun_path` holds 108 bytes including the terminating NUL.
pub const MAX_PATH: usize = 107;
/// How long a connection may take to send its request line, and to take
/// its reply.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections served at once; more wait in the listen backlog.
const CONCURRENT: usize = 4;
/// The longest profile name a remove request may carry.
const MAX_PROFILE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRequest {
    /// Re-read the role document merged with engines.yaml and publish it.
    Add,
    /// Retire a published profile on the server (phase one of `engine
    /// remove`). The role writes nothing; the CLI rewrites engines.yaml once
    /// this is confirmed and then sends `Add` to publish the removal
    /// (review decision C1).
    Remove { profile: String, drain: bool },
    /// Report what the role has published and what uses it.
    List,
}

impl ControlRequest {
    pub fn to_line(&self) -> String {
        match self {
            Self::Add => json!({"v": 1, "op": "add"}),
            Self::Remove { profile, drain } => {
                json!({"v": 1, "op": "remove", "profile": profile, "drain": drain})
            }
            Self::List => json!({"v": 1, "op": "list"}),
        }
        .to_string()
    }

    /// ADR 0018 §3: version 1 only, the three operations only, and no field
    /// beyond the ones each operation names.
    pub fn parse_line(line: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(line).map_err(|_| "not a JSON object".to_owned())?;
        let object = value.as_object().ok_or("not a JSON object")?;
        if object.get("v") != Some(&json!(1)) {
            return Err("unsupported request version".into());
        }
        let known = |keys: &[&str]| object.keys().all(|k| keys.contains(&k.as_str()));
        match object.get("op").and_then(Value::as_str) {
            Some("add") if known(&["v", "op"]) => Ok(Self::Add),
            Some("list") if known(&["v", "op"]) => Ok(Self::List),
            Some("remove") if known(&["v", "op", "profile", "drain"]) => {
                let profile = object
                    .get("profile")
                    .and_then(Value::as_str)
                    .ok_or("remove needs a profile")?;
                let drain = object
                    .get("drain")
                    .and_then(Value::as_bool)
                    .ok_or("remove needs drain")?;
                if profile.is_empty() || profile.len() > MAX_PROFILE {
                    return Err("invalid profile name".into());
                }
                Ok(Self::Remove {
                    profile: profile.into(),
                    drain,
                })
            }
            _ => Err("unknown operation".into()),
        }
    }
}

/// Answers one parsed request. Every reply is `{"ok": true, ...}` or
/// `{"ok": false, "code": "<closed code>", "message": "..."}`.
#[async_trait::async_trait]
pub trait ControlHandler: Send + Sync + 'static {
    async fn handle(&self, request: ControlRequest) -> Value;
}

#[derive(Debug)]
pub enum ControlError {
    PathTooLong(PathBuf),
    InUse(PathBuf),
    Occupied(PathBuf),
    /// The directory holding the socket is not this user's, mode 0700.
    UnsafeDirectory(PathBuf),
    Io(String),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathTooLong(p) => write!(
                f,
                "{} is longer than the {MAX_PATH}-byte socket path limit; use a shorter state_dir",
                p.display()
            ),
            Self::InUse(p) => write!(f, "{} is served by another running role", p.display()),
            Self::Occupied(p) => write!(
                f,
                "{} exists and is not a socket owned by this user; it was left untouched",
                p.display()
            ),
            Self::UnsafeDirectory(p) => write!(
                f,
                "{} must be a directory owned by this user with mode 0700; the socket was not bound",
                p.display()
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ControlError {}

fn own_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// ADR 0018 §3 (review decision C1): the role serves the user id running
/// it and root. Under the system units the operator runs `sudo capyctl engine
/// …`; root can already read and write everything the role owns, so
/// admitting it widens nothing.
fn peer_admitted(peer: u32, expected: u32) -> bool {
    peer == expected || peer == 0
}

/// ADR 0018 §3 (review decision C1): a client speaks to a socket served by
/// its own user id. Root (`sudo capyctl engine …`) speaks to the role's service
/// user instead: the owner of the private (0700) directory holding the socket,
/// which is the role's state directory; no other user could have bound there.
fn server_trusted(own: u32, peer: u32, directory_owner: Option<u32>) -> bool {
    peer == own || (own == 0 && directory_owner == Some(peer))
}

/// The owner of `socket`'s directory, if it is a private (0700) directory.
fn private_directory_owner(socket: &Path) -> Option<u32> {
    let meta = std::fs::symlink_metadata(socket.parent()?).ok()?;
    (meta.file_type().is_dir() && meta.mode() & 0o7777 == 0o700).then(|| meta.uid())
}

pub struct ControlServer {
    path: PathBuf,
    /// `(st_dev, st_ino)` of the socket this server created, so shutdown
    /// removes only that file and never whatever replaced it.
    identity: (u64, u64),
    listener: tokio::net::UnixListener,
}

impl ControlServer {
    /// Bind `path`, refused unless its directory is this user's with mode
    /// 0700. A stale socket (ours, and refusing connections) is
    /// replaced; a live one, a socket of another user, a symlink or any other
    /// file is refused and left alone.
    pub fn bind(path: &Path) -> Result<Self, ControlError> {
        if path.as_os_str().len() > MAX_PATH {
            return Err(ControlError::PathTooLong(path.to_path_buf()));
        }
        let io = |e: std::io::Error| ControlError::Io(format!("{}: {e}", path.display()));
        // ADR 0018 §3: the socket lives only inside the role's private state
        // directory: owned by the user running capyctl, mode 0700, not a link.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| ControlError::UnsafeDirectory(path.to_path_buf()))?;
        match std::fs::symlink_metadata(parent) {
            Ok(meta)
                if meta.file_type().is_dir()
                    && meta.uid() == own_uid()
                    && meta.mode() & 0o7777 == 0o700 => {}
            _ => return Err(ControlError::UnsafeDirectory(parent.to_path_buf())),
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                // symlink_metadata never follows a link, so a symlink here is
                // Occupied rather than a socket.
                if !meta.file_type().is_socket() || meta.uid() != own_uid() {
                    return Err(ControlError::Occupied(path.to_path_buf()));
                }
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => return Err(ControlError::InUse(path.to_path_buf())),
                    // Only a refused connection proves nothing listens; any
                    // other failure leaves the file where it is.
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                        std::fs::remove_file(path).map_err(io)?
                    }
                    Err(e) => return Err(io(e)),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        let std_listener = std::os::unix::net::UnixListener::bind(path).map_err(io)?;
        // The umask may leave the new socket wider for a moment; the peer
        // user-id check below is what admits a connection either way.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
        let meta = std::fs::symlink_metadata(path).map_err(io)?;
        std_listener.set_nonblocking(true).map_err(io)?;
        let listener = tokio::net::UnixListener::from_std(std_listener).map_err(io)?;
        Ok(Self {
            path: path.to_path_buf(),
            identity: (meta.dev(), meta.ino()),
            listener,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serve until `shutdown` turns true (or its sender is gone), then remove
    /// the socket if it is still the one this server bound.
    pub async fn serve(
        self,
        handler: Arc<dyn ControlHandler>,
        expected_uid: u32,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let permits = Arc::new(tokio::sync::Semaphore::new(CONCURRENT));
        loop {
            if *shutdown.borrow_and_update() {
                break;
            }
            let accepted = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() { break }
                    continue;
                }
                accepted = self.listener.accept() => accepted,
            };
            let Ok((stream, _)) = accepted else { continue };
            // ADR 0018 §3: only the user id running capyctl (and root); anyone
            // else is closed unanswered, before anything is read.
            if !matches!(stream.peer_cred(), Ok(cred) if peer_admitted(cred.uid(), expected_uid)) {
                drop(stream);
                continue;
            }
            let permit = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() { break }
                    continue;
                }
                permit = permits.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let _permit = permit;
                serve_one(stream, handler).await;
            });
        }
        if matches!(std::fs::symlink_metadata(&self.path),
            Ok(meta) if meta.file_type().is_socket() && (meta.dev(), meta.ino()) == self.identity)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn serve_one(stream: tokio::net::UnixStream, handler: Arc<dyn ControlHandler>) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(MAX_LINE as u64 + 1));
    let read = tokio::time::timeout(IO_TIMEOUT, reader.read_line(&mut line)).await;
    let reply = match read {
        Ok(Ok(_)) if line.len() <= MAX_LINE && line.ends_with('\n') => {
            match ControlRequest::parse_line(line.trim_end()) {
                Ok(request) => handler.handle(request).await,
                Err(message) => {
                    json!({"ok": false, "code": "invalid_request", "message": message})
                }
            }
        }
        _ => json!({
            "ok": false,
            "code": "invalid_request",
            "message": "one request line of at most 64 KiB"
        }),
    };
    let mut text = reply.to_string();
    if text.len() >= MAX_LINE {
        // A reply is one line of at most 64 KiB; one that would not fit is
        // replaced rather than cut into invalid JSON.
        text = json!({
            "ok": false,
            "code": "reply_too_large",
            "message": "the reply exceeds 64 KiB"
        })
        .to_string();
    }
    text.push('\n');
    let _ = tokio::time::timeout(IO_TIMEOUT, async {
        write.write_all(text.as_bytes()).await?;
        write.shutdown().await
    })
    .await;
}

#[derive(Debug)]
pub enum ClientError {
    /// No role is running here: nothing listens on the socket (it does not
    /// exist, or a stale one refuses the connection). Nothing was asked. This
    /// is the first run, before any role has started, not a fault.
    NotRunning(String),
    /// The request never reached a role: nothing was asked.
    Unreachable(String),
    /// review decision I4: the role took the request, then gave no usable
    /// answer (it closed the connection, answered something that is not a
    /// reply, or the bound passed). What it did is unknown.
    Unanswered(String),
    /// The bound passed before the request was sent: nothing was asked.
    TimedOut,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning(m) | Self::Unreachable(m) | Self::Unanswered(m) => f.write_str(m),
            Self::TimedOut => f.write_str("the role's socket did not take the request in time"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Send one request and read its reply within `timeout`. The socket must be
/// served by this same user id (or, for root, by the owner of the socket's
/// private directory); a socket of anyone else is not spoken to.
pub async fn request(
    path: &Path,
    request: &ControlRequest,
    timeout: Duration,
) -> Result<Value, ClientError> {
    if path.as_os_str().len() > MAX_PATH {
        return Err(ClientError::Unreachable(format!(
            "{} is longer than the {MAX_PATH}-byte socket path limit",
            path.display()
        )));
    }
    let unreachable =
        |e: std::io::Error| ClientError::Unreachable(format!("{}: {e}", path.display()));
    // Set once the whole request line is written: from then on the role may
    // be acting on it, so a failure is `Unanswered`, never `Unreachable`.
    let sent = std::cell::Cell::new(false);
    let exchange = async {
        // No socket, or a stale one nobody listens on: no role is running.
        let mut stream =
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                        ClientError::NotRunning(format!("{}: {e}", path.display()))
                    }
                    _ => unreachable(e),
                })?;
        let directory_owner = private_directory_owner(path);
        if !matches!(stream.peer_cred(),
            Ok(cred) if server_trusted(own_uid(), cred.uid(), directory_owner))
        {
            return Err(ClientError::Unreachable(format!(
                "{} is not served by this user",
                path.display()
            )));
        }
        // A partial write may already have delivered the line; only a
        // failure before any of it left is certainly unasked.
        stream
            .write_all(format!("{}\n", request.to_line()).as_bytes())
            .await
            .map_err(|e| ClientError::Unanswered(format!("{}: {e}", path.display())))?;
        sent.set(true);
        let mut line = String::new();
        BufReader::new(stream.take(MAX_LINE as u64))
            .read_line(&mut line)
            .await
            .map_err(|e| ClientError::Unanswered(e.to_string()))?;
        if line.is_empty() {
            return Err(ClientError::Unanswered(
                "the role closed the connection without answering".into(),
            ));
        }
        serde_json::from_str(line.trim_end())
            .map_err(|_| ClientError::Unanswered("the role's answer is not JSON".into()))
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) if sent.get() => Err(ClientError::Unanswered(
            "the role did not answer in time".into(),
        )),
        Err(_) => Err(ClientError::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T37 (ADR 0018 §3; review decision C1): `sudo capyctl engine …` reaches
    // the role running as its service user; no one else does, and root never
    // trusts a socket in a directory another user could have bound in.
    #[test]
    fn root_and_the_service_user_reach_each_other_and_no_one_else() {
        let (service, operator, root) = (990, 1000, 0);
        // The role, running as the service user, admits itself and root.
        assert!(peer_admitted(service, service));
        assert!(peer_admitted(root, service));
        assert!(!peer_admitted(operator, service));
        // The client trusts its own user id.
        assert!(server_trusted(operator, operator, None));
        assert!(!server_trusted(operator, service, Some(service)));
        // Root trusts the owner of the socket's private directory only.
        assert!(server_trusted(root, service, Some(service)));
        assert!(!server_trusted(root, service, Some(operator)));
        assert!(!server_trusted(root, service, None));
    }

    #[test]
    fn the_directory_owner_is_read_only_from_a_private_directory() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(SOCKET_NAME);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(private_directory_owner(&socket), Some(own_uid()));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        assert_eq!(private_directory_owner(&socket), None);
    }
}
