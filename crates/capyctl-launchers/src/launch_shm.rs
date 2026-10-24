//! ADR 0033 A5: the shared memory an LMCache launch's engines leave behind.
//!
//! Found live on host A (SGLang 0.5.22 and vLLM 0.32.0 with LMCache 0.5.6):
//! with the LMCache connector on, an engine process (SGLang's scheduler,
//! vLLM's engine core) makes the CUDA driver file
//! `cuda.shm.<uid hex>.<pid hex>.1`, which the launch's LMCache server maps
//! through CUDA IPC, so the engine's own cleanup rightly leaves it while the
//! server runs, and a `torch_<pid>_<n>_<n>` file that vLLM never removes. A
//! `restart_only` park keeps the server; once the server's verified stop ended
//! its hold, nothing removed either file. Without LMCache neither is made.
//!
//! SPEC §13.2: ownership is evidence, never a name pattern alone. A file is
//! removed only after a verified stop of an LMCache launch's groups, when its
//! name carries the pid of an engine process the stop recorded and proved
//! gone, it is still the same file (device and inode) as when first seen, a
//! regular file owned by this user with no group or other access, and no live
//! process this user can inspect holds it. The launch's own processes are all
//! proven gone by then; a process that hides its mappings could only have
//! opened the file by name. A file the stopped engine left that a live process
//! still holds (its server, through a restart-park) is recorded at first
//! sighting with its holders, as the SGLang entry's watch records, and removed
//! by the same rules after the verified stop of a server that was one of them.
//!
//! That record is kept in this process's memory: a role restarted between a
//! restart-park and the server's stop keeps nothing, and leaves those files.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use capyctl_domain::completion::ProcessIdentity;

/// A file as the kernel names it: (device, inode).
type FileKey = (u64, u64);

/// A process that held a file, as `/proc` named it then: pid and start ticks
/// (0 when they could not be read; never a recorded identity's).
type Holder = (u32, u64);

/// A file an engine's verified stop left held by a live process.
struct Deferred {
    path: PathBuf,
    key: FileKey,
    holders: Vec<Holder>,
}

/// Every such file this process has seen and not yet settled.
static DEFERRED: Mutex<Vec<Deferred>> = Mutex::new(Vec::new());

/// Once a stop of an LMCache launch's groups has proven every identity of
/// `identities` gone: remove the files named for its exited engine processes
/// and, when it stopped a server, the files earlier engine stops recorded as
/// held by that server, each one nothing holds now. A file a live process
/// still holds after a stop of the engine alone (a restart-park) is recorded
/// with its holders instead.
pub(crate) fn after_verified_stop(root: &Path, identities: &[ProcessIdentity]) {
    let Some(prefix) = prefix(root) else {
        return;
    };
    let (servers, engines): (Vec<_>, Vec<_>) = identities
        .iter()
        .partition(|identity| identity.is_lmcache());
    let mut candidates = engine_files(root, &engines);
    if !servers.is_empty() {
        let stopped: HashSet<Holder> = servers
            .iter()
            .map(|server| (server.pid, server.start_ticks))
            .collect();
        lock().retain(|record| {
            if record.holders.iter().any(|holder| stopped.contains(holder)) {
                candidates.push((record.path.clone(), record.key));
                return false;
            }
            // A record whose file is gone or replaced settles nothing.
            private_key(&record.path) == Some(record.key)
        });
    }
    if candidates.is_empty() {
        return;
    }
    let targets = candidates.iter().map(|(_, key)| *key).collect();
    let Some(holding) = holders(&prefix, &targets) else {
        return;
    };
    let mut deferred = Vec::new();
    for (path, key) in candidates {
        if private_key(&path) != Some(key) {
            continue;
        }
        if let Some(holders) = holding.get(&key) {
            if servers.is_empty() {
                deferred.push(Deferred {
                    path,
                    key,
                    holders: holders.clone(),
                });
            }
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!(
                    "the shared-memory file {} a stopped engine left was not removed: {error}",
                    path.display()
                ),
            ),
        }
    }
    lock().extend(deferred);
}

fn lock() -> std::sync::MutexGuard<'static, Vec<Deferred>> {
    DEFERRED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The files in `root` named for one of `engines`' pids, each a private
/// regular file of this user.
fn engine_files(root: &Path, engines: &[&ProcessIdentity]) -> Vec<(PathBuf, FileKey)> {
    if engines.is_empty() {
        return Vec::new();
    }
    let uid = nix::unistd::geteuid().as_raw();
    let pids: HashSet<u32> = engines.iter().map(|identity| identity.pid).collect();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| creator(name, uid))
                .is_some_and(|pid| pids.contains(&pid))
        })
        .filter_map(|entry| {
            let path = entry.path();
            private_key(&path).map(|key| (path, key))
        })
        .collect()
}

/// The pid a file's name says created it: the CUDA driver's
/// `cuda.shm.<uid hex>.<pid hex>.1` for this `uid`, or torch's
/// `torch_<pid>_<n>_<n>`. Only the exact form either writes is read.
fn creator(name: &str, uid: u32) -> Option<u32> {
    if let Some(pid) = name
        .strip_prefix(&format!("cuda.shm.{uid:x}."))
        .and_then(|rest| rest.strip_suffix(".1"))
    {
        let parsed = u32::from_str_radix(pid, 16).ok()?;
        return (format!("{parsed:x}") == pid).then_some(parsed);
    }
    let mut parts = name.strip_prefix("torch_")?.split('_');
    let (pid, first, second) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if parts.next().is_some() || !digits(first) || !digits(second) {
        return None;
    }
    let parsed: u32 = pid.parse().ok()?;
    (parsed.to_string() == pid).then_some(parsed)
}

/// (device, inode) of a regular file (never followed through a symlink) this
/// user owns with no group or other access.
fn private_key(path: &Path) -> Option<FileKey> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    (metadata.file_type().is_file()
        && metadata.uid() == nix::unistd::geteuid().as_raw()
        && metadata.mode() & 0o077 == 0)
        .then(|| (metadata.dev(), metadata.ino()))
}

/// `root` resolved (mappings and descriptors name the resolved path), with a
/// trailing slash.
fn prefix(root: &Path) -> Option<String> {
    let resolved = std::fs::canonicalize(root).ok()?;
    Some(format!("{}/", resolved.to_str()?.trim_end_matches('/')))
}

/// The files under `prefix` process `pid` has mapped or open; nothing when
/// `/proc` does not show them.
fn held(pid: u32, prefix: &str) -> Vec<FileKey> {
    let base = format!("/proc/{pid}");
    let mut keys = Vec::new();
    if let Ok(maps) = std::fs::read_to_string(format!("{base}/maps")) {
        for line in maps.lines() {
            // address perms offset dev inode path
            let fields: Vec<&str> = line.split_ascii_whitespace().take(6).collect();
            let &[_, _, _, dev, inode, path] = fields.as_slice() else {
                continue;
            };
            if !path.starts_with(prefix) {
                continue;
            }
            let device = dev.split_once(':').and_then(|(major, minor)| {
                Some(nix::sys::stat::makedev(
                    u64::from_str_radix(major, 16).ok()?,
                    u64::from_str_radix(minor, 16).ok()?,
                ))
            });
            if let (Some(device), Ok(inode)) = (device, inode.parse()) {
                keys.push((device, inode));
            }
        }
    }
    if let Ok(descriptors) = std::fs::read_dir(format!("{base}/fd")) {
        for descriptor in descriptors.flatten() {
            let path = descriptor.path();
            let opens = std::fs::read_link(&path)
                .ok()
                .and_then(|target| target.to_str().map(|t| t.starts_with(prefix)))
                .unwrap_or(false);
            if !opens {
                continue;
            }
            if let Ok(metadata) = std::fs::metadata(&path) {
                keys.push((metadata.dev(), metadata.ino()));
            }
        }
    }
    keys
}

/// Every live process that holds one of `targets`; `None` when `/proc` cannot
/// be listed (then nothing is known unheld).
fn holders(prefix: &str, targets: &HashSet<FileKey>) -> Option<HashMap<FileKey, Vec<Holder>>> {
    let mut found: HashMap<FileKey, Vec<Holder>> = HashMap::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let keys: HashSet<FileKey> = held(pid, prefix)
            .into_iter()
            .filter(|key| targets.contains(key))
            .collect();
        if keys.is_empty() {
            continue;
        }
        let ticks = crate::subreaper::proc_stat(pid).map_or(0, |stat| stat.start_ticks);
        for key in keys {
            found.entry(key).or_default().push((pid, ticks));
        }
    }
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::creator;

    // T45 (ADR 0033 A5): only the exact names the CUDA driver and torch write
    // name a creator, and only the CUDA driver's for this uid.
    #[test]
    fn only_the_exact_driver_and_torch_names_carry_a_creator() {
        assert_eq!(creator("cuda.shm.3e8.6fca.1", 0x3e8), Some(0x6fca));
        assert_eq!(creator("torch_28618_2793364754_0", 0x3e8), Some(28618));
        for name in [
            "cuda.shm.3e9.6fca.1",
            "cuda.shm.3e8.06fca.1",
            "cuda.shm.3e8.6FCA.1",
            "cuda.shm.3e8.6fca.2",
            "cuda.shm.3e8..1",
            "torch_028618_1_0",
            "torch_28618_1",
            "torch_28618_1_0_0",
            "torch_28618_x_0",
            "torch__1_0",
            "sglang_loads_c3ddbf4a3e2e_84b4f33a.shm",
        ] {
            assert_eq!(creator(name, 0x3e8), None, "{name}");
        }
    }
}
