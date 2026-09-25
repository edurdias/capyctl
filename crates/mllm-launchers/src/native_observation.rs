//! Read-only scheduler allocation observations; never lifecycle authority.
//!
//! Requires trusted Linux procfs and service custody of the socket and connected
//! descriptors. Same-UID processes are not a security boundary. This synchronous
//! client belongs on a bounded blocking executor, not an async reactor thread.
//! Kernel filesystem reads cannot be deadline-bounded. Socket waits share one
//! deadline (at most two seconds); there is no retry or lifecycle side effect.
//!
//! Key mode (`with_key`, protocol version 2) proves each request with the
//! launch's observation key instead of relying on the listener having enrolled
//! this process's PID, so a restarted host observes the launch it owns (T33).

use mllm_domain::completion::ProcessIdentity;
use nix::libc;
use serde::Deserialize;
use std::{
    fs,
    io::{Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
#[error("native allocation observation unavailable")]
pub struct ObservationError;
type Result<T> = std::result::Result<T, ObservationError>;

/// Which step of an observation failed, on the host's own log: a fixed stage
/// name only, never a path, key, proof or engine output. Found live
/// 2026-09-23 (M28): every failure read alike and an SGLang park could only
/// be refused as unchanged, with no trace of why.
fn staged(stage: &'static str) -> ObservationError {
    eprintln!(
        "{}",
        serde_json::json!({"event": "native_observation_failed", "stage": stage})
    );
    ObservationError
}

/// A socket stage's failure with what the wire showed, on one line: why the
/// wait ended (`deadline`, `eof` or `error` with its errno), the bytes received
/// of those expected, the time since the observation began against its budget,
/// and whether the enrolled scheduler was still alive afterwards. Counts and
/// times only, never a path, key, proof or engine output. Found live
/// 2026-09-24 (rc.2, M28 s92-14): a park was refused at `receive_header` and
/// the bare stage could not tell a slow scheduler from a closed connection.
fn staged_io(stage: &'static str, failure: Failure<'_>) -> ObservationError {
    let (cause, errno) = match failure.cause {
        Cause::Deadline => ("deadline", None),
        Cause::Eof => ("eof", None),
        Cause::Error(errno) => ("error", Some(errno)),
    };
    let owner_alive = crate::exec::process_identity(failure.owner.pid, &failure.owner.role)
        .as_ref()
        == Some(failure.owner);
    eprintln!(
        "{}",
        serde_json::json!({
            "event": "native_observation_failed",
            "stage": stage,
            "cause": cause,
            "errno": errno,
            "received": failure.received,
            "expected": failure.expected,
            "elapsed_ms": failure.started.elapsed().as_millis() as u64,
            "timeout_ms": failure.timeout_ms as u64,
            "owner_alive": owner_alive,
        })
    );
    ObservationError
}

/// A successful observation that took a third of its budget or more.
fn slow(elapsed: Duration, timeout: Duration) -> bool {
    elapsed.saturating_mul(3) >= timeout
}

/// The wire facts `staged_io` reports.
struct Failure<'a> {
    cause: Cause,
    received: usize,
    expected: usize,
    started: Instant,
    timeout_ms: u128,
    owner: &'a ProcessIdentity,
}

/// Immutable service-provisioned endpoint. Construction does not enroll a worker.
pub struct NativeObservationClient {
    path: PathBuf,
    socket_identity: (u64, u64),
    binding_id: String,
    incarnation_id: String,
    owner: ProcessIdentity,
    library_sha256: String,
    /// The per-launch observation key (version 2); `None` is version 1.
    key: Option<[u8; 32]>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllocationTag {
    Weights,
    KvCache,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationGroup {
    pub device: u32,
    pub tag: AllocationTag,
    pub allocation_count: u64,
    pub active_count: u64,
    pub paused_count: u64,
    pub virtual_bytes: u64,
    pub mapped_bytes: u64,
    pub backup_bytes: u64,
    pub backup_enabled_count: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocations {
    pub groups: Vec<AllocationGroup>,
    pub allocation_count: u64,
    pub virtual_bytes: u64,
    pub mapped_bytes: u64,
    pub backup_bytes: u64,
}
/// Point-in-time saver-map facts only. No global idleness or residency claim.
#[derive(Debug)]
pub struct AllocationFacts {
    pub binding_id: String,
    pub incarnation_id: String,
    pub request_id: String,
    pub owner: ProcessIdentity,
    pub library_sha256: String,
    pub started_ns: u64,
    pub finished_ns: u64,
    pub allocations: Allocations,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    pid: u32,
    start_ticks: u64,
    boot_id: String,
}
impl Identity {
    fn matches(&self, expected: &ProcessIdentity) -> bool {
        self.pid == expected.pid
            && self.start_ticks == expected.start_ticks
            && self.boot_id == expected.boot_id
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    owner: Identity,
    library_sha256: String,
    hook_mode: String,
    allocations: Allocations,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    binding_id: String,
    incarnation_id: String,
    request_id: String,
    owner: Identity,
    started_ns: u64,
    finished_ns: u64,
    status: String,
    observation: Snapshot,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}
fn protected_socket(path: &Path) -> Result<(u64, u64)> {
    let text = path.to_str().ok_or(ObservationError)?;
    if !path.is_absolute()
        || text.len() > 107
        || text.chars().any(char::is_control)
        || text
            .split('/')
            .skip(1)
            .any(|p| p.is_empty() || p == "." || p == "..")
        || fs::canonicalize(path).map_err(|_| ObservationError)? != path
    {
        return Err(ObservationError);
    }
    let uid = unsafe { libc::geteuid() };
    if uid != unsafe { libc::getuid() } {
        return Err(ObservationError);
    }
    let leaf = fs::symlink_metadata(path).map_err(|_| ObservationError)?;
    if !leaf.file_type().is_socket()
        || leaf.uid() != uid
        || leaf.mode() & 0o022 != 0
        || leaf.nlink() != 1
    {
        return Err(ObservationError);
    }
    for parent in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(parent).map_err(|_| ObservationError)?;
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || metadata.mode() & 0o022 != 0
        {
            return Err(ObservationError);
        }
    }
    Ok((leaf.dev(), leaf.ino()))
}
fn monotonic_ns() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(ObservationError);
    }
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|v| v.checked_mul(1_000_000_000))
        .and_then(|v| v.checked_add(time.tv_nsec.try_into().ok()?))
        .ok_or(ObservationError)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(ObservationError)
}
/// Why a socket wait or read ended without its bytes. Diagnostic only: every
/// cause fails the observation closed the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    /// The one shared deadline passed first.
    Deadline,
    /// The peer closed the connection (EOF, or hang-up without data).
    Eof,
    /// A poll or read error, with its errno.
    Error(i32),
}
impl Cause {
    fn last_os_error() -> Self {
        Self::Error(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}
fn wait_cause(
    stream: &UnixStream,
    events: i16,
    deadline: Instant,
) -> std::result::Result<(), Cause> {
    loop {
        let duration = remaining(deadline).map_err(|_| Cause::Deadline)?;
        let millis = duration.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        let mut fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        let count = unsafe { libc::poll(&mut fd, 1, millis) };
        if count > 0 {
            remaining(deadline).map_err(|_| Cause::Deadline)?;
            return Ok(());
        }
        if count == 0 {
            return Err(Cause::Deadline);
        }
        let cause = Cause::last_os_error();
        if cause != Cause::Error(libc::EINTR) {
            return Err(cause);
        }
    }
}
fn wait(stream: &UnixStream, events: i16, deadline: Instant) -> Result<()> {
    wait_cause(stream, events, deadline).map_err(|_| ObservationError)
}
fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(ObservationError);
    }
    let stream = unsafe { UnixStream::from_raw_fd(raw) };
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    for (dst, src) in address
        .sun_path
        .iter_mut()
        .zip(path.as_os_str().as_encoded_bytes())
    {
        *dst = *src as _;
    }
    let code = unsafe {
        libc::connect(
            raw,
            (&address as *const libc::sockaddr_un).cast(),
            mem::size_of_val(&address) as _,
        )
    };
    if code != 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(ObservationError);
        }
        wait(&stream, libc::POLLOUT, deadline)?;
        if stream.take_error().map_err(|_| ObservationError)?.is_some() {
            return Err(ObservationError);
        }
    }
    remaining(deadline)?;
    Ok(stream)
}
/// Fill `bytes` before the deadline; on failure, why and how many bytes came.
fn receive(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    deadline: Instant,
) -> std::result::Result<(), (Cause, usize)> {
    let mut offset = 0;
    while offset < bytes.len() {
        wait_cause(stream, libc::POLLIN, deadline).map_err(|cause| (cause, offset))?;
        match stream.read(&mut bytes[offset..]) {
            Ok(0) => return Err((Cause::Eof, offset)),
            Ok(n) => offset += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err((Cause::Error(e.raw_os_error().unwrap_or(0)), offset)),
        }
    }
    Ok(())
}
fn peer(stream: &UnixStream, owner: &ProcessIdentity) -> Result<()> {
    let mut credentials: libc::ucred = unsafe { mem::zeroed() };
    let mut length = mem::size_of_val(&credentials) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
        || length as usize != mem::size_of_val(&credentials)
        || credentials.pid <= 0
        || credentials.pid as u32 != owner.pid
        || credentials.uid != unsafe { libc::geteuid() }
        || unsafe { libc::getuid() } != credentials.uid
    {
        return Err(ObservationError);
    }
    let before = crate::exec::process_identity(owner.pid, &owner.role).ok_or(ObservationError)?;
    // The shared identity helper does not reject zombies; corroborate live state.
    let mut stat = String::new();
    fs::File::open(format!("/proc/{}/stat", owner.pid))
        .map_err(|_| ObservationError)?
        .take(16385)
        .read_to_string(&mut stat)
        .map_err(|_| ObservationError)?;
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, s)| s.split_whitespace().next())
        .ok_or(ObservationError)?;
    if stat.len() > 16384 || matches!(state, "Z" | "X" | "x") {
        return Err(ObservationError);
    }
    // CLOCK_MONOTONIC values are comparable only in the same time namespace.
    let local_ns = fs::metadata("/proc/self/ns/time").map_err(|_| ObservationError)?;
    let peer_ns =
        fs::metadata(format!("/proc/{}/ns/time", owner.pid)).map_err(|_| ObservationError)?;
    if (local_ns.dev(), local_ns.ino()) != (peer_ns.dev(), peer_ns.ino())
        || before != *owner
        || crate::exec::process_identity(owner.pid, &owner.role).as_ref() != Some(owner)
    {
        return Err(ObservationError);
    }
    Ok(())
}
impl NativeObservationClient {
    pub fn new(
        path: PathBuf,
        binding_id: String,
        incarnation_id: String,
        owner: ProcessIdentity,
        library_sha256: String,
    ) -> Result<Self> {
        if !identifier(&binding_id)
            || !identifier(&incarnation_id)
            || owner.pid == 0
            || owner.pid > i32::MAX as u32
            || owner.start_ticks == 0
            || library_sha256.len() != 64
            || !library_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ObservationError);
        }
        let socket_identity = protected_socket(&path)?;
        Ok(Self {
            path,
            socket_identity,
            binding_id,
            incarnation_id,
            owner,
            library_sha256,
            key: None,
        })
    }
    /// Prove every request with the launch's observation key (version 2).
    pub fn with_key(mut self, key: [u8; 32]) -> Self {
        self.key = Some(key);
        self
    }
    /// One fresh correlation ID and one connection. Any incomplete result fails closed.
    pub fn observe(&self, timeout: Duration) -> Result<AllocationFacts> {
        let timeout_ms = timeout.as_millis();
        if !(1..=2000).contains(&timeout_ms) || timeout != Duration::from_millis(timeout_ms as u64)
        {
            return Err(staged("timeout_shape"));
        }
        let begun = Instant::now();
        let deadline = begun + timeout;
        let started = monotonic_ns()?;
        if protected_socket(&self.path).map_err(|_| staged("socket_before"))?
            != self.socket_identity
        {
            return Err(staged("socket_replaced_before"));
        }
        let failure = |cause, received, expected| Failure {
            cause,
            received,
            expected,
            started: begun,
            timeout_ms,
            owner: &self.owner,
        };
        let mut stream = connect(&self.path, deadline).map_err(|_| staged("connect"))?;
        peer(&stream, &self.owner).map_err(|_| staged("peer_before"))?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let version = if self.key.is_some() { 2 } else { 1 };
        let body = match &self.key {
            Some(key) => serde_json::json!({
                "version": 2,
                "request_id": request_id,
                "timeout_ms": timeout_ms,
                "proof": mllm_adapters::sglang::observation::request_proof(
                    key, &self.binding_id, &self.incarnation_id, &request_id),
            }),
            None => {
                serde_json::json!({"version":1,"request_id":request_id,"timeout_ms":timeout_ms})
            }
        };
        let request = serde_json::to_vec(&body).map_err(|_| ObservationError)?;
        if request.len() > 1024 {
            return Err(ObservationError);
        }
        let mut frame = (request.len() as u32).to_be_bytes().to_vec();
        frame.extend(request);
        let mut sent = 0;
        while sent < frame.len() {
            wait_cause(&stream, libc::POLLOUT, deadline)
                .map_err(|cause| staged_io("send", failure(cause, sent, frame.len())))?;
            match stream.write(&frame[sent..]) {
                Ok(0) => return Err(staged_io("send", failure(Cause::Eof, sent, frame.len()))),
                Ok(n) => sent += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    let cause = Cause::Error(e.raw_os_error().unwrap_or(0));
                    return Err(staged_io("send", failure(cause, sent, frame.len())));
                }
            }
        }
        let mut header = [0; 4];
        receive(&mut stream, &mut header, deadline).map_err(|(cause, received)| {
            staged_io("receive_header", failure(cause, received, header.len()))
        })?;
        let count = u32::from_be_bytes(header) as usize;
        if !(1..=65536).contains(&count) {
            return Err(staged("frame_size"));
        }
        let mut data = vec![0; count];
        receive(&mut stream, &mut data, deadline).map_err(|(cause, received)| {
            staged_io("receive_body", failure(cause, received, count))
        })?;
        // A full frame alone is insufficient: exact EOF rules out trailing frames.
        loop {
            wait_cause(&stream, libc::POLLIN, deadline)
                .map_err(|cause| staged_io("receive_eof", failure(cause, 0, 0)))?;
            match stream.read(&mut [0; 1]) {
                Ok(0) => break,
                Ok(_) => return Err(staged("trailing_bytes")),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    let cause = Cause::Error(e.raw_os_error().unwrap_or(0));
                    return Err(staged_io("receive_eof", failure(cause, 0, 0)));
                }
            }
        }
        peer(&stream, &self.owner).map_err(|_| staged("peer_after"))?;
        if protected_socket(&self.path)? != self.socket_identity {
            return Err(staged("socket_replaced_after"));
        }
        let finished = monotonic_ns()?;
        let response: Response =
            serde_json::from_slice(&data).map_err(|_| staged("response_shape"))?;
        if response.version != version
            || response.binding_id != self.binding_id
            || response.incarnation_id != self.incarnation_id
            || response.request_id != request_id
            || !response.owner.matches(&self.owner)
            || response.status != "observed"
            || response.started_ns < started
            || response.finished_ns < response.started_ns
            || response.finished_ns > finished
            || !response.observation.owner.matches(&self.owner)
            || response.observation.library_sha256 != self.library_sha256
            || response.observation.hook_mode != "preload"
        {
            return Err(staged(if response.status != "observed" {
                "response_status"
            } else {
                "response_binding"
            }));
        }
        let allocations = response.observation.allocations;
        validate_allocations(&allocations).map_err(|_| staged("allocations_shape"))?;
        remaining(deadline).map_err(|_| staged("deadline"))?;
        let elapsed = begun.elapsed();
        if slow(elapsed, timeout) {
            // A success that used a third of its budget or more: the stalls
            // that end in `receive_header` show up here first (an idle
            // scheduler answers in tens of milliseconds, measured live
            // 2026-09-25), and the engine's own log is off by default.
            eprintln!(
                "{}",
                serde_json::json!({
                    "event": "native_observation_slow",
                    "elapsed_ms": elapsed.as_millis() as u64,
                    "timeout_ms": timeout_ms as u64,
                })
            );
        }
        Ok(AllocationFacts {
            binding_id: response.binding_id,
            incarnation_id: response.incarnation_id,
            request_id,
            owner: self.owner.clone(),
            library_sha256: self.library_sha256.clone(),
            started_ns: response.started_ns,
            finished_ns: response.finished_ns,
            allocations,
        })
    }
}
fn validate_allocations(value: &Allocations) -> Result<()> {
    if value.groups.len() > 4096 || value.allocation_count > 4096 || value.backup_bytes != 0 {
        return Err(ObservationError);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut sums = [0u64; 3];
    for group in &value.groups {
        if group.device > i32::MAX as u32
            || !seen.insert((group.device, matches!(group.tag, AllocationTag::Weights)))
            || !(1..=4096).contains(&group.allocation_count)
            || group.active_count > 4096
            || group.paused_count > 4096
            || group.active_count + group.paused_count != group.allocation_count
            || group.virtual_bytes == 0
            || group.mapped_bytes > group.virtual_bytes
            || group.backup_bytes != 0
            || group.backup_enabled_count != 0
        {
            return Err(ObservationError);
        }
        for (sum, v) in sums.iter_mut().zip([
            group.allocation_count,
            group.virtual_bytes,
            group.mapped_bytes,
        ]) {
            *sum = sum.checked_add(v).ok_or(ObservationError)?;
        }
    }
    if sums
        != [
            value.allocation_count,
            value.virtual_bytes,
            value.mapped_bytes,
        ]
    {
        return Err(ObservationError);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
    #[test]
    fn interoperates_with_actual_python_transport_without_engine_imports() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let dir = tempfile::Builder::new()
            .prefix("mllm-observer-")
            .tempdir_in(std::env::var("HOME").unwrap())
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("observe.sock");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let mut child = Command::new("python3")
            .args(["-I", "-B"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/native_observation_server.py"),
            )
            .arg(root)
            .arg(&path)
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let wire: Identity = serde_json::from_str(&line).unwrap();
        let owner = ProcessIdentity {
            role: "scheduler".into(),
            pid: wire.pid,
            start_ticks: wire.start_ticks,
            boot_id: wire.boot_id,
        };
        let client = NativeObservationClient::new(
            path,
            "binding".into(),
            "incarnation".into(),
            owner,
            "a".repeat(64),
        )
        .unwrap();
        let result = client.observe(Duration::from_millis(1000));
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        let facts = result.unwrap();
        assert!(status.success());
        assert_eq!(facts.allocations.allocation_count, 2);
        assert_eq!(facts.allocations.virtual_bytes, 8192);
        assert_eq!(facts.allocations.mapped_bytes, 0);
        assert_eq!(facts.allocations.groups[0].tag, AllocationTag::KvCache);
    }
    /// T33: key mode against the actual Python listener. A client holding the
    /// launch's key observes whatever its PID; one with another key is denied
    /// without the bridge being asked.
    #[test]
    fn interoperates_with_actual_python_transport_in_key_mode() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let dir = tempfile::Builder::new()
            .prefix("mllm-observer-")
            .tempdir_in(std::env::var("HOME").unwrap())
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("observe.sock");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let mut child = Command::new("python3")
            .args(["-I", "-B"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/native_observation_server.py"),
            )
            .arg(root)
            .arg(&path)
            .arg("0")
            .arg("admin-key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let wire: Identity = serde_json::from_str(&line).unwrap();
        let owner = ProcessIdentity {
            role: "scheduler".into(),
            pid: wire.pid,
            start_ticks: wire.start_ticks,
            boot_id: wire.boot_id,
        };
        let client = |admin: &str| {
            NativeObservationClient::new(
                path.clone(),
                "binding".into(),
                "incarnation".into(),
                owner.clone(),
                "a".repeat(64),
            )
            .unwrap()
            .with_key(mllm_adapters::sglang::observation::observation_key(
                admin,
                "binding",
                "incarnation",
            ))
        };
        let denied = client("another-key").observe(Duration::from_millis(1000));
        let result = client("admin-key").observe(Duration::from_millis(1000));
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert!(denied.is_err());
        let facts = result.unwrap();
        assert!(status.success());
        assert_eq!(facts.allocations.virtual_bytes, 8192);
        assert_eq!(facts.allocations.mapped_bytes, 0);
    }
    fn fixture(
        change: impl FnOnce(&mut serde_json::Value) + Send + 'static,
        trailing: bool,
    ) -> Result<AllocationFacts> {
        fixture_wire(
            change,
            move |bytes| {
                let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
                frame.extend(bytes);
                if trailing {
                    frame.push(b'x');
                }
                frame
            },
            Duration::ZERO,
        )
    }
    fn fixture_wire(
        change: impl FnOnce(&mut serde_json::Value) + Send + 'static,
        encode: impl FnOnce(Vec<u8>) -> Vec<u8> + Send + 'static,
        hold_open: Duration,
    ) -> Result<AllocationFacts> {
        let dir = tempfile::Builder::new()
            .prefix("mllm-observer-")
            .tempdir_in(std::env::var("HOME").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("observe.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let identity = crate::exec::process_identity(std::process::id(), "scheduler").unwrap();
        let owner = identity.clone();
        let client = NativeObservationClient::new(
            path,
            "binding".into(),
            "incarnation".into(),
            identity,
            "a".repeat(64),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut bytes).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let now = monotonic_ns().unwrap();
            let identity = serde_json::json!({"pid":owner.pid,"boot_id":owner.boot_id,"start_ticks":owner.start_ticks});
            let mut response = serde_json::json!({"version":1,"binding_id":"binding","incarnation_id":"incarnation","request_id":request["request_id"],"owner":identity,"started_ns":now,"finished_ns":now,"status":"observed","observation":{"owner":identity,"library_sha256":"a".repeat(64),"hook_mode":"preload","allocations":{"groups":[{"device":0,"tag":"weights","allocation_count":1,"active_count":1,"paused_count":0,"virtual_bytes":4096,"mapped_bytes":4096,"backup_bytes":0,"backup_enabled_count":0}],"allocation_count":1,"virtual_bytes":4096,"mapped_bytes":4096,"backup_bytes":0}}});
            change(&mut response);
            let bytes = serde_json::to_vec(&response).unwrap();
            let _ = stream.write_all(&encode(bytes));
            std::thread::sleep(hold_open);
        });
        let result = client.observe(Duration::from_millis(500));
        server.join().unwrap();
        result
    }
    #[test]
    fn accepts_only_scoped_correlated_allocation_facts() {
        let facts = fixture(|_| {}, false).unwrap();
        assert_eq!(facts.allocations.mapped_bytes, 4096);
        assert_eq!(facts.allocations.groups[0].tag, AllocationTag::Weights);
    }
    #[test]
    fn empty_map_is_an_allocation_observation_not_a_lifecycle_state() {
        let facts = fixture(|v| {
            v["observation"]["allocations"] = serde_json::json!({"groups":[], "allocation_count":0, "virtual_bytes":0, "mapped_bytes":0, "backup_bytes":0});
        }, false).unwrap();
        assert!(facts.allocations.groups.is_empty());
        assert_eq!(facts.allocations.allocation_count, 0);
        assert_eq!(facts.binding_id, "binding");
    }
    #[test]
    fn denies_malformed_unbound_or_unsafe_observations() {
        for (pointer, value) in [
            ("/status", serde_json::json!("uncertain")),
            ("/binding_id", serde_json::json!("other")),
            ("/request_id", serde_json::json!("replay")),
            ("/owner/start_ticks", serde_json::json!(1)),
            ("/started_ns", serde_json::json!(0)),
            (
                "/observation/library_sha256",
                serde_json::json!("b".repeat(64)),
            ),
            ("/observation/hook_mode", serde_json::json!("late")),
            (
                "/observation/allocations/groups/0/tag",
                serde_json::json!("unknown"),
            ),
            (
                "/observation/allocations/groups/0/backup_bytes",
                serde_json::json!(1),
            ),
            (
                "/observation/allocations/mapped_bytes",
                serde_json::json!(1),
            ),
            (
                "/observation/allocations/groups/0/active_count",
                serde_json::json!(2),
            ),
        ] {
            assert!(
                fixture(move |v| *v.pointer_mut(pointer).unwrap() = value, false).is_err(),
                "{pointer}"
            );
        }
        assert!(fixture(
            |v| {
                v["extra"] = serde_json::json!(0);
            },
            false
        )
        .is_err());
        assert!(fixture(|_| {}, true).is_err());
    }
    #[test]
    fn denies_unprotected_or_missing_socket() {
        let identity = crate::exec::process_identity(std::process::id(), "scheduler").unwrap();
        assert!(NativeObservationClient::new(
            "/tmp/missing.sock".into(),
            "binding".into(),
            "incarnation".into(),
            identity,
            "a".repeat(64)
        )
        .is_err());
    }
    #[test]
    fn denies_duplicate_nested_fields_partial_oversized_and_invalid_frames() {
        for bytes in [
            vec![0, 0, 0, 0],
            vec![0, 1, 0, 1],
            vec![0, 0, 0],
            vec![0, 0, 0, 3, b'{'],
            vec![0, 0, 0, 1, 255],
        ] {
            assert!(fixture_wire(|_| {}, move |_| bytes, Duration::ZERO).is_err());
        }
        for field in ["version", "pid", "mapped_bytes"] {
            assert!(fixture_wire(
                |_| {},
                move |bytes| {
                    let text = String::from_utf8(bytes).unwrap();
                    let needle = format!("\"{field}\":");
                    let text = text.replacen(&needle, &format!("\"{field}\":0,{needle}"), 1);
                    let mut frame = (text.len() as u32).to_be_bytes().to_vec();
                    frame.extend(text.bytes());
                    frame
                },
                Duration::ZERO
            )
            .is_err());
        }
    }
    #[test]
    fn full_frame_without_eof_is_not_success_and_has_one_total_deadline() {
        let start = Instant::now();
        assert!(fixture_wire(
            |_| {},
            |bytes| {
                let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
                frame.extend(bytes);
                frame
            },
            Duration::from_millis(650)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_millis(1500));
    }
    /// The wire facts a failed socket stage reports: a peer that closes shows
    /// EOF with the bytes it sent, a silent one the shared deadline.
    #[test]
    fn receive_reports_why_and_how_many_bytes_arrived() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        server.write_all(&[0, 0]).unwrap();
        let mut header = [0; 4];
        let deadline = Instant::now() + Duration::from_millis(100);
        assert_eq!(
            receive(&mut client, &mut header, deadline),
            Err((Cause::Deadline, 2))
        );
        drop(server);
        let deadline = Instant::now() + Duration::from_millis(100);
        assert_eq!(
            receive(&mut client, &mut header, deadline),
            Err((Cause::Eof, 0))
        );
    }
    #[test]
    fn slow_is_a_third_of_the_budget_or_more() {
        let budget = Duration::from_millis(1500);
        assert!(!slow(Duration::from_millis(17), budget));
        assert!(!slow(Duration::from_millis(499), budget));
        assert!(slow(Duration::from_millis(500), budget));
        assert!(slow(Duration::from_millis(1499), budget));
    }
    #[test]
    fn denies_unsafe_socket_alias_replacement_and_changed_owner_identity() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::Builder::new()
            .prefix("mllm-observer-")
            .tempdir_in(std::env::var("HOME").unwrap())
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("observe.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let owner = crate::exec::process_identity(std::process::id(), "scheduler").unwrap();
        let make = |p, o| {
            NativeObservationClient::new(
                p,
                "binding".into(),
                "incarnation".into(),
                o,
                "a".repeat(64),
            )
        };
        let alias = dir.path().join("alias");
        symlink(&path, &alias).unwrap();
        assert!(make(alias, owner.clone()).is_err());
        let client = make(path.clone(), owner.clone()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o622)).unwrap();
        assert!(client.observe(Duration::from_millis(50)).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut stale_owner = owner.clone();
        stale_owner.start_ticks += 1;
        assert!(make(path.clone(), stale_owner)
            .unwrap()
            .observe(Duration::from_millis(50))
            .is_err());
        let mut wrong_pid = owner.clone();
        wrong_pid.pid = 1;
        assert!(make(path.clone(), wrong_pid)
            .unwrap()
            .observe(Duration::from_millis(50))
            .is_err());
        fs::rename(&path, dir.path().join("old.sock")).unwrap();
        let _replacement = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(client.observe(Duration::from_millis(50)).is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o722)).unwrap();
        assert!(make(path, owner).is_err());
    }
}
