//! Owner decision 2026-10-20 (SPEC §13.3 / T21): `engine_logs: memory` keeps a
//! launch's redacted engine output only in its log writer's memory.
//!
//! The writer (`capyctl_launchers::engine_log_relay`) holds the last
//! [`MEMORY_BYTES`] of the launch's redacted output in a [`Ring`] and answers
//! reads on a Linux abstract Unix socket named from the launch's log path
//! ([`address`]), so nothing is written to disk, not even a socket file. Both
//! ends refuse a peer that is not the same user ([`same_user`]): the abstract
//! namespace is visible to every user of the network namespace, so the user
//! check is what keeps another account from reading the buffer or posing as
//! its writer. The writer keeps answering for [`LINGER`] after the engine
//! exits, so a launch that failed is still explained from its own output.
//!
//! One request per connection: an operation byte, then a little-endian `u32`
//! byte limit. A tail is answered with one byte (1 when older output exists
//! before the text) and the bytes, then end of stream; a release with one
//! byte (1 when the writer's input had ended and it is leaving).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The most redacted output a launch keeps in memory.
pub const MEMORY_BYTES: usize = 4 * 1024 * 1024;

/// How long the writer keeps its buffer after the engine's output ended.
pub const LINGER: Duration = Duration::from_secs(60);

/// The bound on one read: connect, ask and receive at most a tail.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Read the end of the buffer.
pub const TAIL: u8 = b'T';

/// Ask a writer whose engine has exited to leave now, so a new launch of the
/// same log can take its address.
pub const RELEASE: u8 = b'R';

/// The abstract socket a launch's writer answers on: a digest of the log
/// path, so any path length fits and the name says nothing about the launch.
pub fn address(log: &Path) -> std::io::Result<SocketAddr> {
    use std::os::unix::ffi::OsStrExt;
    let digest = hex::encode(Sha256::digest(log.as_os_str().as_bytes()));
    SocketAddr::from_abstract_name(format!("capyctl-engine-log/{digest}"))
}

/// Whether the process at the other end of `stream` runs as this user.
pub fn same_user(stream: &UnixStream) -> bool {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    getsockopt(stream, PeerCredentials)
        .is_ok_and(|peer| peer.uid() == nix::unistd::geteuid().as_raw())
}

/// Up to `limit` bytes from the end of the launch's buffer, and whether older
/// output exists before them. `NotFound` when no writer of this user answers
/// for the log.
pub fn read_tail(log: &Path, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut stream = connect(log)?;
    request(&mut stream, TAIL, limit)?;
    let mut answer = Vec::new();
    stream.take(limit as u64 + 1).read_to_end(&mut answer)?;
    let Some((&flag, bytes)) = answer.split_first() else {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    };
    Ok((bytes.to_vec(), flag == 1))
}

/// Ask the writer of `log`, if one answers, to leave now because its engine
/// has exited. Whether it said it is leaving.
pub fn release(log: &Path) -> bool {
    let Ok(mut stream) = connect(log) else {
        return false;
    };
    let mut answer = [0u8; 1];
    request(&mut stream, RELEASE, 0).is_ok()
        && stream.read_exact(&mut answer).is_ok()
        && answer[0] == 1
}

fn connect(log: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect_addr(&address(log)?).map_err(|error| {
        match error.kind() {
            // No writer answers: there is no buffer for this log.
            std::io::ErrorKind::ConnectionRefused => std::io::ErrorKind::NotFound.into(),
            _ => error,
        }
    })?;
    if !same_user(&stream) {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

fn request(stream: &mut UnixStream, operation: u8, limit: usize) -> std::io::Result<()> {
    let mut message = [0u8; 5];
    message[0] = operation;
    message[1..].copy_from_slice(&u32::try_from(limit).unwrap_or(u32::MAX).to_le_bytes());
    stream.write_all(&message)
}

/// Read one request from a connected reader: its operation and byte limit.
pub fn read_request(stream: &mut UnixStream) -> std::io::Result<(u8, usize)> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut message = [0u8; 5];
    stream.read_exact(&mut message)?;
    let limit = u32::from_le_bytes([message[1], message[2], message[3], message[4]]);
    Ok((message[0], limit as usize))
}

/// Answer a tail request with `bytes` and whether older output exists before
/// them ([`Ring::tail`], taken before writing so a slow reader never holds
/// the buffer).
pub fn answer_tail(stream: &mut UnixStream, bytes: &[u8], truncated: bool) -> std::io::Result<()> {
    stream.write_all(&[u8::from(truncated)])?;
    stream.write_all(bytes)
}

/// The last [`MEMORY_BYTES`] (or `capacity`) of a launch's redacted output,
/// whole lines only: older output leaves from the front a line at a time.
pub struct Ring {
    bytes: VecDeque<u8>,
    capacity: usize,
    dropped: bool,
}

impl Default for Ring {
    fn default() -> Self {
        Self::with_capacity(MEMORY_BYTES)
    }
}

impl Ring {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            capacity,
            dropped: false,
        }
    }

    /// Append `line` (its own line ending included).
    pub fn push(&mut self, line: &[u8]) {
        let line = &line[line.len().saturating_sub(self.capacity)..];
        self.bytes.extend(line);
        if self.bytes.len() > self.capacity {
            self.bytes.drain(..self.bytes.len() - self.capacity);
            // Keep the buffer starting at a line boundary.
            match self.bytes.iter().position(|b| *b == b'\n') {
                Some(end) => drop(self.bytes.drain(..=end)),
                None => self.bytes.clear(),
            }
            self.dropped = true;
        }
    }

    /// Up to `limit` bytes from the end, and whether older output exists
    /// before them.
    pub fn tail(&self, limit: usize) -> (Vec<u8>, bool) {
        let start = self.bytes.len().saturating_sub(limit);
        (
            self.bytes.range(start..).copied().collect(),
            self.dropped || start > 0,
        )
    }
}
