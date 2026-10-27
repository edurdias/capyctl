//! SPEC §9.2: the production SGLang saver observation source, shared by the
//! remote host agent (W4) and the embedded standalone host (W5).
//!
//! A memory-saver launch enrolls its scheduler's observation in this host's
//! private directory (`runtime/sglang_observation_enrollment.py`): a key-mode
//! Unix socket `<binding>.sock` and a record `<binding>.json` naming the
//! binding, incarnation, the scheduler's process identity and the saver
//! library's path and digest. ADR 0035 §9: a launch on N > 1 GPUs runs one
//! scheduler per GPU, rank r on CUDA ordinal r, and each enrolls its own map:
//! rank 0 under the binding's names, rank r > 0 under `<binding>.rank<r>.json`
//! and `<binding>.rank<r>.sock`. Each rank's read here:
//!
//! 1. takes that rank's record only from the private directory (0700, this
//!    service user) as an owner-only regular file, scoped to exactly this
//!    binding and incarnation and naming that rank's own socket;
//! 2. requires the enrolled scheduler to be one of the launch's recorded
//!    processes (or, before a step names them, a live process of this boot),
//!    and no two ranks to be the same process;
//! 3. hashes the saver library itself, which must be the preload build inside
//!    the launch's own installation, and calls the saver real only when that
//!    digest is the one the scheduler reported;
//! 4. asks the scheduler over the socket with the launch's observation key
//!    (derived from its admin credential, so a restarted host still can), and
//!    sums mapped and virtual saver bytes per tag on its own device: the single
//!    device of a launch on one GPU, device r for rank r of a launch on N.
//!
//! A launch on N GPUs is read whole: every rank, in rank order, and any rank
//! unreadable makes the whole read unknown (ADR 0035 §9).
//!
//! Nothing here is release, restoration or readiness evidence by itself: the
//! observers below fuse it with process liveness, quiescence and the step
//! sequence, and any unreadable fact is unknown, never a convenient default.
use super::residency::{SaverMapped, SaverResidency, SaverScope, SaverUnavailable};
use capyctl_adapters::{
    sglang::{ObservationAccess, SglangRuntimeObservation, SglangRuntimeObserver},
    traits::{Quiescence, RuntimeAction, RuntimeCommand, RuntimeError, Unproven},
};
use capyctl_domain::completion::{ExecutionIdentities, Presence, ProcessIdentity};
use capyctl_launchers::native_observation::{AllocationTag, NativeObservationClient};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// One saver read, connect through response (the scheduler answers at its
/// next safe point). The protocol's largest bound: found live 2026-09-26, a
/// busy SGLang scheduler on a discrete-GPU laptop answered in 1.0-1.6 s.
const OBSERVE_TIMEOUT: Duration = Duration::from_millis(2000);
/// A read has no effect, so one that misses its bound is made again with a
/// fresh request id (SPEC §9.2: only a whole, bound answer is evidence; a
/// missing one is never guessed). Bounded well inside any step deadline.
const OBSERVE_ATTEMPTS: usize = 3;
const MAX_RECORD_BYTES: u64 = 4096;
const MAX_LIBRARY_BYTES: u64 = 64 * 1024 * 1024;
const PRELOAD_PREFIX: &str = "torch_memory_saver_hook_mode_preload";
/// ADR 0035 §9: the most ranks one launch reads (one scheduler per GPU of one
/// host).
const MAX_RANKS: u32 = 64;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    pid: u32,
    start_ticks: u64,
    boot_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    binding_id: String,
    incarnation_id: String,
    owner: Owner,
    library_path: String,
    library_sha256: String,
    hook_mode: String,
    socket: String,
}

/// The enrolled-scheduler saver source over one private directory.
pub struct EnrolledSaver {
    dir: PathBuf,
}

impl EnrolledSaver {
    /// `dir` is this host's private observation directory; the role creates
    /// it 0700 before any launch.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn uid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    fn private_dir(&self) -> Result<(), SaverUnavailable> {
        let metadata = std::fs::symlink_metadata(&self.dir).map_err(|_| SaverUnavailable)?;
        if !metadata.is_dir() || metadata.uid() != Self::uid() || metadata.mode() & 0o777 != 0o700 {
            return Err(SaverUnavailable);
        }
        Ok(())
    }

    /// The record of one enrollment (`enrollment_name`), never any other path.
    fn record(&self, name: &str) -> Result<Record, SaverUnavailable> {
        if !enrollment(name) {
            return Err(SaverUnavailable);
        }
        self.private_dir()?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.dir.join(format!("{name}.json")))
            .map_err(|_| SaverUnavailable)?;
        let metadata = file.metadata().map_err(|_| SaverUnavailable)?;
        if !metadata.is_file()
            || metadata.uid() != Self::uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.len() > MAX_RECORD_BYTES
        {
            return Err(SaverUnavailable);
        }
        let mut text = String::new();
        (&mut file)
            .take(MAX_RECORD_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|_| SaverUnavailable)?;
        serde_json::from_str(&text).map_err(|_| SaverUnavailable)
    }
}

/// Why a saver read could not be made, on the host's own log (a fixed stage
/// name, never a path, key or engine output). Found live 2026-09-23: an SGLang
/// park refused as unchanged left no trace of which precondition failed.
fn unavailable(stage: &'static str) -> SaverUnavailable {
    capyctl_domain::role_log::event(
        serde_json::json!({"event": "saver_observation_unavailable", "stage": stage}),
    );
    SaverUnavailable
}

/// A binding or incarnation is a ULID-shaped token; never a path.
fn token(value: &str) -> bool {
    (1..=128).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// ADR 0035 §9: the enrollment name of one rank's scheduler, as
/// `runtime/sglang_observation_enrollment.py` (`enrollment_name`) writes it:
/// the binding itself for rank 0 (and every launch on one GPU), and
/// `<binding>.rank<r>` for rank r of a launch on several GPUs.
fn enrollment_name(binding: &str, rank: u32) -> String {
    if rank == 0 {
        binding.to_owned()
    } else {
        format!("{binding}.rank{rank}")
    }
}

/// An enrollment name is a binding token, alone or with the fixed `.rank<r>`
/// suffix of a rank in 1..MAX_RANKS written as `enrollment_name` writes it;
/// never a path.
fn enrollment(name: &str) -> bool {
    match name.split_once(".rank") {
        None => token(name),
        Some((binding, rank)) => {
            token(binding)
                && rank.parse::<u32>().is_ok_and(|rank| {
                    (1..MAX_RANKS).contains(&rank) && enrollment_name(binding, rank) == name
                })
        }
    }
}

/// The record names exactly this launch and this enrollment's own socket.
fn record_matches(record: &Record, scope: &SaverScope, name: &str) -> bool {
    record.version == 1
        && record.binding_id == scope.binding_id
        && record.incarnation_id == scope.incarnation
        && record.hook_mode == "preload"
        && record.socket == format!("{name}.sock")
        && record.library_sha256.len() == 64
}

/// The enrolled scheduler must be one of the launch's own recorded processes;
/// before a step names them, a live process of this boot.
fn enrolled_owner(
    record: &Record,
    scope: &SaverScope,
) -> Result<ProcessIdentity, SaverUnavailable> {
    let matches = |identity: &ProcessIdentity| {
        identity.pid == record.owner.pid
            && identity.start_ticks == record.owner.start_ticks
            && identity.boot_id == record.owner.boot_id
    };
    match &scope.members {
        Some(members) => members
            .iter()
            .find(|identity| matches(identity))
            .cloned()
            .ok_or_else(|| unavailable("owner_not_a_launch_member")),
        None => {
            let owner = ProcessIdentity {
                role: "scheduler".into(),
                pid: record.owner.pid,
                start_ticks: record.owner.start_ticks,
                boot_id: record.owner.boot_id.clone(),
            };
            if capyctl_launchers::process_absence::presence(&owner) != Presence::Alive {
                return Err(unavailable("owner_not_alive"));
            }
            Ok(owner)
        }
    }
}

/// One rank's map covers its own GPU only. On one GPU, at most one device
/// (the single-rank recipe: a spread map cannot prove release or
/// restoration). ADR 0035 §9: on N GPUs the engine sees only the launch's
/// GPUs in rank order (`base_gpu_id` 0, step 1), so rank r's scheduler runs on
/// CUDA ordinal r and every allocation group of its map must be on device r.
fn rank_devices(
    devices: impl IntoIterator<Item = u32>,
    ranks: u32,
    rank: u32,
) -> Result<(), SaverUnavailable> {
    let devices = devices
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if ranks <= 1 {
        if devices.len() > 1 {
            return Err(unavailable("multiple_devices"));
        }
    } else if devices.iter().any(|device| *device != rank) {
        return Err(unavailable("rank_device"));
    }
    Ok(())
}

/// The saver library must be the preload build inside the launch's own
/// installation (`<prefix>/lib/python*/site-packages/`), and its digest is
/// measured here. `Ok(false)`: a real file whose bytes are not the ones the
/// scheduler reported loading.
fn library_is_real(record: &Record, executable: &str) -> Result<bool, SaverUnavailable> {
    let library = Path::new(&record.library_path);
    let prefix = Path::new(executable)
        .parent()
        .and_then(Path::parent)
        .ok_or(SaverUnavailable)?
        .canonicalize()
        .map_err(|_| SaverUnavailable)?;
    let name = library
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SaverUnavailable)?;
    let site = library.parent().ok_or(SaverUnavailable)?;
    let python = site.parent().ok_or(SaverUnavailable)?;
    if !library.is_absolute()
        || library.canonicalize().map_err(|_| SaverUnavailable)? != library
        || !name.starts_with(PRELOAD_PREFIX)
        || !name.ends_with(".so")
        || site.file_name().and_then(|n| n.to_str()) != Some("site-packages")
        || !python
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("python"))
        || python.parent() != Some(prefix.join("lib").as_path())
    {
        return Err(SaverUnavailable);
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(library)
        .map_err(|_| SaverUnavailable)?;
    let metadata = file.metadata().map_err(|_| SaverUnavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_LIBRARY_BYTES {
        return Err(SaverUnavailable);
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|_| SaverUnavailable)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()) == record.library_sha256)
}

impl EnrolledSaver {
    /// One rank's enrollment, bound to this launch: its record (named for
    /// that rank, naming that rank's own socket) and its scheduler, a launch
    /// member (or, with no members yet, alive). No socket is touched.
    fn enrolled(
        &self,
        scope: &SaverScope,
        rank: u32,
    ) -> Result<(Record, ProcessIdentity), SaverUnavailable> {
        if !token(&scope.binding_id)
            || !token(&scope.incarnation)
            || !(1..=MAX_RANKS).contains(&scope.ranks)
            || rank >= scope.ranks
        {
            return Err(SaverUnavailable);
        }
        let name = enrollment_name(&scope.binding_id, rank);
        let record = self
            .record(&name)
            .map_err(|_| unavailable("record_unreadable"))?;
        if !record_matches(&record, scope, &name) {
            return Err(unavailable("record_mismatch"));
        }
        let owner = enrolled_owner(&record, scope)?;
        Ok((record, owner))
    }

    /// ADR 0035 §9: every rank's enrollment, in rank order, before any rank is
    /// asked. Any rank missing or unbound makes the whole launch unknown, and
    /// so does one process enrolled under two ranks: each GPU's scheduler is
    /// its own process, so a shared owner is not every GPU's evidence.
    fn enrolled_ranks(
        &self,
        scope: &SaverScope,
    ) -> Result<Vec<(Record, ProcessIdentity)>, SaverUnavailable> {
        if !(1..=MAX_RANKS).contains(&scope.ranks) {
            return Err(unavailable("ranks"));
        }
        let enrolled = (0..scope.ranks)
            .map(|rank| self.enrolled(scope, rank))
            .collect::<Result<Vec<_>, _>>()?;
        let owners = enrolled
            .iter()
            .map(|(_, owner)| (owner.pid, owner.start_ticks, owner.boot_id.as_str()))
            .collect::<std::collections::BTreeSet<_>>();
        if owners.len() != enrolled.len() {
            return Err(unavailable("shared_owner"));
        }
        Ok(enrolled)
    }

    /// Ask one enrolled rank's scheduler for its map, on its own device.
    fn observe_rank(
        &self,
        scope: &SaverScope,
        rank: u32,
        record: &Record,
        owner: ProcessIdentity,
    ) -> Result<SaverMapped, SaverUnavailable> {
        let real_saver = library_is_real(record, &scope.executable)
            .map_err(|_| unavailable("library_unverifiable"))?;
        let key = capyctl_adapters::sglang::observation::observation_key(
            &scope.admin_key,
            &scope.binding_id,
            &scope.incarnation,
        );
        let client = NativeObservationClient::new(
            self.dir.join(&record.socket),
            scope.binding_id.clone(),
            scope.incarnation.clone(),
            owner,
            record.library_sha256.clone(),
        )
        .map_err(|_| unavailable("client"))?
        .with_key(key);
        let facts = (1..OBSERVE_ATTEMPTS)
            .find_map(|_| client.observe(OBSERVE_TIMEOUT).ok())
            .map_or_else(|| client.observe(OBSERVE_TIMEOUT), Ok)
            .map_err(|_| unavailable("observe"))?;
        rank_devices(
            facts.allocations.groups.iter().map(|g| g.device),
            scope.ranks,
            rank,
        )?;
        let sum = |tag: AllocationTag, mapped: bool| {
            facts
                .allocations
                .groups
                .iter()
                .filter(|g| g.tag == tag)
                .map(|g| {
                    if mapped {
                        g.mapped_bytes
                    } else {
                        g.virtual_bytes
                    }
                })
                .sum::<u64>()
        };
        Ok(SaverMapped {
            real_saver,
            weight_bytes: sum(AllocationTag::Weights, true),
            kv_bytes: sum(AllocationTag::KvCache, true),
            weight_virtual_bytes: sum(AllocationTag::Weights, false),
            kv_virtual_bytes: sum(AllocationTag::KvCache, false),
        })
    }

    /// One rank's whole read: its enrollment, then its map.
    fn read_rank(&self, scope: &SaverScope, rank: u32) -> Result<SaverMapped, SaverUnavailable> {
        let (record, owner) = self.enrolled(scope, rank)?;
        self.observe_rank(scope, rank, &record, owner)
    }
}

impl SaverResidency for EnrolledSaver {
    fn mapped(&self, scope: &SaverScope) -> Result<SaverMapped, SaverUnavailable> {
        // ADR 0035 §9: one rank's map is never a launch on several GPUs.
        if scope.ranks > 1 {
            return Err(unavailable("multiple_ranks"));
        }
        self.read_rank(scope, 0)
    }

    /// ADR 0035 §9: every rank's map, in rank order; any rank unreadable makes
    /// the whole read unknown.
    fn mapped_ranks(&self, scope: &SaverScope) -> Result<Vec<SaverMapped>, SaverUnavailable> {
        self.enrolled_ranks(scope)?
            .into_iter()
            .zip(0u32..)
            .map(|((record, owner), rank)| self.observe_rank(scope, rank, &record, owner))
            .collect()
    }

    fn observation_dir(&self) -> Option<&Path> {
        Some(&self.dir)
    }

    fn unenrolled(&self, binding_id: &str) -> bool {
        token(binding_id)
            && self.private_dir().is_ok()
            && std::fs::symlink_metadata(self.dir.join(format!("{binding_id}.json")))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    }

    fn retire(&self, binding_id: &str) {
        // Only this user's own records and sockets for exactly that binding,
        // every rank's (ADR 0035 §9); anything else in the directory is left
        // as it is.
        if !token(binding_id) || self.private_dir().is_err() {
            return;
        }
        for rank in 0..MAX_RANKS {
            let name = enrollment_name(binding_id, rank);
            for file in [format!("{name}.json"), format!("{name}.sock")] {
                let path = self.dir.join(file);
                let owned = std::fs::symlink_metadata(&path).is_ok_and(|m| {
                    use std::os::unix::fs::FileTypeExt;
                    m.uid() == Self::uid() && (m.is_file() || m.file_type().is_socket())
                });
                if owned {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

/// The saver facts one observation stands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SaverFacts {
    pub real_saver: bool,
    /// Every saver allocation of both tags is mapped.
    pub resident: bool,
    /// No saver allocation of either tag is mapped.
    pub released: bool,
}

/// SPEC §9.2, T20: only a whole map is evidence. Missing pools, a tag partly
/// mapped, or one tag resident while the other is released is partial evidence:
/// unknown, which closes every action and leaves an effect uncertain.
///
/// ADR 0014 amendment A17: a launch whose weights stay resident is released
/// when its KV cache is unmapped and its weights are still fully mapped; for
/// it, released weights are the partial map.
///
/// ADR 0035 §9: `mapped` holds one map per rank (one per GPU), each judged
/// alone as above. The park settles only when, on every chosen GPU, the
/// weights and KV regions are unmapped; the wake only when they are mapped on
/// every GPU. No ranks, any rank unjudgeable, or ranks that disagree (some
/// resident, some released) are unknown. The saver is real only when every
/// rank's is.
pub(super) fn saver_facts(
    mapped: &Result<Vec<SaverMapped>, SaverUnavailable>,
    resident_weights: bool,
) -> Option<SaverFacts> {
    let (first, rest) = mapped.as_ref().ok()?.split_first()?;
    let mut facts = rank_facts(first, resident_weights)?;
    for rank in rest {
        let next = rank_facts(rank, resident_weights)?;
        if (next.resident, next.released) != (facts.resident, facts.released) {
            return None;
        }
        facts.real_saver &= next.real_saver;
    }
    Some(facts)
}

/// One rank's map judged alone (one GPU's whole map).
fn rank_facts(mapped: &SaverMapped, resident_weights: bool) -> Option<SaverFacts> {
    if mapped.weight_virtual_bytes == 0
        || mapped.kv_virtual_bytes == 0
        || mapped.weight_bytes > mapped.weight_virtual_bytes
        || mapped.kv_bytes > mapped.kv_virtual_bytes
    {
        return None;
    }
    let weights_mapped = mapped.weight_bytes == mapped.weight_virtual_bytes;
    let resident = weights_mapped && mapped.kv_bytes == mapped.kv_virtual_bytes;
    let released = mapped.kv_bytes == 0
        && if resident_weights {
            weights_mapped
        } else {
            mapped.weight_bytes == 0
        };
    (resident || released).then_some(SaverFacts {
        real_saver: mapped.real_saver,
        resident,
        released,
    })
}

/// Content validity (weights, cache) before and after one step. Contents are
/// never physically provable: they follow the step sequence, and a released
/// map always falsifies them.
pub(super) fn content(action: RuntimeAction) -> [(bool, bool); 2] {
    match action {
        RuntimeAction::Park => [(true, true), (true, true)],
        RuntimeAction::Restore => [(false, false), (false, false)],
        RuntimeAction::ReloadWeights => [(false, false), (true, false)],
        RuntimeAction::InvalidateCache => [(true, false), (true, true)],
        RuntimeAction::Probe => [(true, true), (true, true)],
        _ => [(false, false), (false, false)],
    }
}

/// SPEC §9.2 (W5): the embedded host's SGLang observer for one launch. It is
/// built once with the launch and binds each persisted step it is asked about:
/// the step's recorded processes (their liveness read now), the saver map, the
/// engine's own gauges (the coordinator has already drained its request leases
/// to zero before a Park, SPEC §10), and the step sequence's content.
pub struct LaunchSglangObserver {
    saver: Arc<dyn SaverResidency>,
    dir: Option<PathBuf>,
}

impl LaunchSglangObserver {
    pub fn new(saver: Arc<dyn SaverResidency>) -> Self {
        let dir = saver.observation_dir().map(Path::to_path_buf);
        Self { saver, dir }
    }

    async fn saver(&self, scope: SaverScope) -> Result<Vec<SaverMapped>, SaverUnavailable> {
        read_saver(&self.saver, scope).await
    }
}

/// One saver read off the async threads: it hashes a file and waits on a socket.
/// ADR 0035 §9: every rank's map, in rank order (one on one GPU).
pub(super) async fn read_saver(
    saver: &Arc<dyn SaverResidency>,
    scope: SaverScope,
) -> Result<Vec<SaverMapped>, SaverUnavailable> {
    let saver = saver.clone();
    tokio::task::spawn_blocking(move || saver.mapped_ranks(&scope))
        .await
        .map_err(|_| SaverUnavailable)?
}

fn scope(
    binding: &str,
    incarnation: &str,
    members: Option<Vec<ProcessIdentity>>,
    access: &ObservationAccess<'_>,
) -> SaverScope {
    SaverScope {
        binding_id: binding.into(),
        incarnation: incarnation.into(),
        members,
        admin_key: access.admin_key.into(),
        executable: access.executable.into(),
        ranks: access.ranks,
    }
}

#[async_trait::async_trait]
impl SglangRuntimeObserver for LaunchSglangObserver {
    async fn observe(&self) -> Result<SglangRuntimeObservation, RuntimeError> {
        // A launch-wide observer answers only for a named step.
        Err(RuntimeError::Unsupported)
    }

    async fn observe_step(
        &self,
        command: &RuntimeCommand,
        after: bool,
        access: &ObservationAccess<'_>,
    ) -> Result<SglangRuntimeObservation, RuntimeError> {
        let c = &command.context;
        let ExecutionIdentities::Retained(expected) = &c.identities else {
            return Err(RuntimeError::Unsupported);
        };
        let recorded = expected.clone();
        let live = tokio::task::spawn_blocking(move || {
            recorded
                .into_iter()
                .filter(|identity| {
                    capyctl_launchers::process_absence::presence(identity) == Presence::Alive
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|_| RuntimeError::Unsupported)?;
        let mapped = self
            .saver(scope(
                &c.binding_id,
                &c.incarnation,
                Some(expected.clone()),
                access,
            ))
            .await;
        let idle = capyctl_adapters::sglang::observation::engine_idle(
            access.endpoint,
            access.inference_key,
        )
        .await;
        let facts = saver_facts(&mapped, access.resident_weights);
        let (weights, cache) = content(command.action)[usize::from(after)];
        Ok(SglangRuntimeObservation {
            token: c.token.clone(),
            binding_id: c.binding_id.clone(),
            incarnation: c.incarnation.clone(),
            identities: live,
            real_memory_saver: facts.is_some_and(|f| f.real_saver),
            quiesced: idle == Ok(true),
            unknown_work: facts.is_none() || idle.is_err(),
            allocations: facts.is_some_and(|f| f.resident),
            weights: weights && facts.is_some_and(|f| f.resident),
            cache: cache && facts.is_some_and(|f| f.resident),
        })
    }

    async fn quiescent(&self, access: &ObservationAccess<'_>) -> Quiescence {
        // SPEC §10 step 4 before an embedded Park: the engine's gauges are at
        // zero and the enrolled saver is real and fully mapped. The Park step
        // then re-proves both against the launch's recorded processes.
        let mapped = self
            .saver(scope(access.binding_id, access.incarnation, None, access))
            .await;
        let idle = capyctl_adapters::sglang::observation::engine_idle(
            access.endpoint,
            access.inference_key,
        )
        .await;
        let quiescent = idle == Ok(true)
            && saver_facts(&mapped, access.resident_weights)
                .is_some_and(|f| f.real_saver && f.resident);
        // Found live 2026-10-14: a launch that never enrolled was refused as
        // merely not quiescent; the refusal now says it is unobserved.
        let unproven = (!quiescent && self.saver.unenrolled(access.binding_id))
            .then_some(Unproven::ObservationNotEnrolled);
        Quiescence {
            quiescent,
            unproven,
        }
    }

    fn observation_dir(&self) -> Option<PathBuf> {
        self.dir.clone()
    }
}

#[cfg(test)]
mod tests;
