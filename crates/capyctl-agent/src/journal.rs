//! SPEC §§13.1–13.3: durable command acceptance is separate from effect authority.
//! At-least-once delivery returns evidence, never a second execution capability.
//! Unknown effects retain their host claim until owned-process cleanup is proven.
use crate::identity_storage::IdentityDirectory;
use capyctl_adapters::traits::{OwnedProcessLaunch, RenderedCommand};
use capyctl_domain::completion::{Presence, ProcessIdentity};
use capyctl_launchers::{
    owned_launch::DurableProcessLaunch, AssociationError, LaunchAssociation,
    ProtectedLaunchDescriptors,
};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand},
    pb,
};
use prost::Message;
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

const MAX_COMMAND_BYTES: usize = 32 * 1024;
const MAX_HISTORY: usize = 256;
const MAX_PROCESSES: usize = 256;
const MAX_HISTORY_BYTES: usize = 256 * 1024;
const MARKER: &str = "journal-established";
const MARKER_BYTES: &[u8] = b"capyctl-host-journal-v1";
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("host journal unavailable or invalid")]
    Storage,
    #[error("host journal is locked by another local process")]
    Busy,
    #[error("command identity or payload conflicts with retained state")]
    Conflict,
    #[error("command is fenced, expired, or disconnected")]
    Fenced,
    #[error("local execution authority is missing or unsupported")]
    Unauthorized,
    #[error("ownership is unresolved and remains retained")]
    Uncertain,
    /// SPEC §13.2 / T33: the journal was written by a newer capyctl. An older
    /// binary never opens it, because it would read and write a schema it does
    /// not know; the journal is left untouched.
    #[error(
        "host journal has schema version {found}, newer than the {supported} this capyctl \
         supports; it was written by a newer capyctl. Run that newer capyctl, or restore the \
         journal from a backup taken before the upgrade. The journal was not modified"
    )]
    FromNewerVersion { found: i64, supported: i64 },
}
impl From<rusqlite::Error> for JournalError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandState {
    Accepted,
    Attempted,
    Launched,
    Completed,
    Tombstone,
}
impl CommandState {
    fn decode(value: i64) -> Result<Self, JournalError> {
        Ok(match value {
            0 => Self::Accepted,
            1 => Self::Attempted,
            2 => Self::Launched,
            3 => Self::Completed,
            4 => Self::Tombstone,
            _ => return Err(JournalError::Storage),
        })
    }
}
#[derive(Clone, Debug)]
pub struct CommandRecord {
    pub sequence: i64,
    pub command_id: String,
    pub state: CommandState,
    /// Historical ownership evidence, including previously observed workers.
    /// Roles may repeat after worker churn. This is NOT current group readiness.
    pub processes: Vec<ProcessIdentity>,
    pub claim_retained: bool,
}
/// This non-cloneable process-local capability cannot be reconstructed from a
/// replay or a database row. Dropping it leaves accepted work for reconciliation.
#[derive(Debug)]
pub struct ExecutionTicket {
    journal: Weak<HostJournal>,
    command_id: String,
    session: u64,
}
#[derive(Debug)]
pub enum Acceptance {
    Fresh(ExecutionTicket),
    Replay(CommandRecord),
}

/// Locally rendered process configuration and private launch descriptors.
/// Neither this value nor its credential-bearing descriptors enter the journal.
pub struct ApprovedLaunch {
    pub command: RenderedCommand,
    pub descriptors: Option<ProtectedLaunchDescriptors>,
}

/// Execution clocks are re-read after local preparation and at the durable
/// child gate. The host session supplies trusted time, never a wire timestamp.
pub trait ExecutionClock: Send + Sync {
    fn now_ms(&self) -> Result<i64, JournalError>;
}
struct AnchoredClock {
    epoch_ms: i64,
    started: Instant,
}
impl ExecutionClock for AnchoredClock {
    fn now_ms(&self) -> Result<i64, JournalError> {
        let elapsed =
            i64::try_from(self.started.elapsed().as_millis()).map_err(|_| JournalError::Fenced)?;
        self.epoch_ms
            .checked_add(elapsed)
            .ok_or(JournalError::Fenced)
    }
}
fn before_deadline(clock: &dyn ExecutionClock, deadline: i64) -> Result<(), JournalError> {
    let now = clock.now_ms()?;
    if now < 0 || now >= deadline {
        Err(JournalError::Fenced)
    } else {
        Ok(())
    }
}

/// Implemented by the host's approved profile and resource-grant registry.
/// `authorize` MUST validate profile fingerprint, expected state, retained grant,
/// local devices, checkpoint/build and permitted overrides. A wire grant's shape
/// or a controller signature alone is not resource admission. Implementations
/// must not perform effects here. There is deliberately no default allow policy.
pub trait LocalExecutionPolicy {
    fn authorize(&self, command: &MemberCommand) -> Result<(), JournalError>;
    /// Render exclusively from locally approved configuration; never deserialize
    /// remote argv/env. Called again just before the effect to recheck authority.
    fn render_launch(&self, command: &MemberCommand) -> Result<ApprovedLaunch, JournalError>;
    /// SPEC §§9.1, 10: whether this host may park or restore `owner`, the
    /// retained launch a Park or Restore names (same member, deployment and
    /// profile, already checked by the journal). The implementation resolves
    /// the owner's approved configuration and admits only its declared tier.
    /// No effect here; the default refuses.
    fn authorize_residency(
        &self,
        _command: &MemberCommand,
        _owner: &MemberCommand,
    ) -> Result<(), JournalError> {
        Err(JournalError::Unauthorized)
    }
    /// SPEC §§3.1, 7.3 (per-launch claims, journal v4): whether a new launch
    /// fits beside every launch this host still claims, judged against the
    /// host's own approved resource policy (memory domain budget, device
    /// sharing, port range). This is defense in depth: the controller remains
    /// the admission authority. `claimed` never includes `command` itself.
    /// `Unauthorized` is a typed policy refusal; `Uncertain` means the host
    /// cannot tell. No effect here, and the policy must not call back into the
    /// journal. The default keeps the single-claim host: a second launch is
    /// refused while any claim is retained.
    fn admit_beside(
        &self,
        _command: &MemberCommand,
        claimed: &[ClaimedLaunch],
    ) -> Result<(), JournalError> {
        if claimed.is_empty() {
            Ok(())
        } else {
            Err(JournalError::Uncertain)
        }
    }
    /// SPEC §§3.1, 7.3, 9.1: whether waking `owner` (the parked launch a
    /// Restore names) fits beside every other launch this host claims, with
    /// the wake charged the owner's ready footprint. Defense in depth beside
    /// the controller's own wake admission. `claimed` never includes `owner`.
    /// `Unauthorized` is a typed policy refusal, after which the launch stays
    /// parked; `Uncertain` means the host cannot tell. No effect here. The
    /// default keeps the single-claim host: no wake beside another claim.
    fn admit_wake(
        &self,
        _command: &MemberCommand,
        _owner: &MemberCommand,
        claimed: &[ClaimedLaunch],
    ) -> Result<(), JournalError> {
        if claimed.is_empty() {
            Ok(())
        } else {
            Err(JournalError::Uncertain)
        }
    }
}

/// SPEC §§3.1, 7.3: what a launch this host still claims may be holding now,
/// for host-side co-residence admission. Derived only from durable journal
/// state; anything short of recorded readiness or a settled park is charged
/// as the launch that is still starting or changing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPhase {
    /// Accepted, attempted or launched without recorded native readiness.
    Starting,
    /// Launched, with native readiness recorded and no residency change.
    Ready,
    /// Parked in place (W4).
    Parked,
    /// Parking, restoring or quarantined: any footprint of its recipe.
    Changing,
}

/// One retained launch claim: the immutable launch command (its deployment,
/// generation and owned handle, which is the command id) and its phase.
#[derive(Clone, Debug)]
pub struct ClaimedLaunch {
    pub command: MemberCommand,
    pub phase: ClaimPhase,
}

/// Journal schema version this build writes. v4 (per-launch claims) replaced
/// the host-wide single claim with one claim per instance incarnation; v5
/// (per-instance fencing) keys command fencing by (deployment, instance).
pub const JOURNAL_SCHEMA_VERSION: i64 = 5;

/// The capability a host advertises in its inventory publication when its
/// journal keeps one claim per launch (`ReportInventory.launch_claims`).
pub const PER_LAUNCH_CLAIMS: &str = "per_launch";

/// ADR 0013 §4, §5 (owner decision P1): the capability this build advertises.
/// A per-launch journal (v5) that also fences each instance of a deployment by
/// that instance's own last assignment, so two instances of one deployment may
/// share this host. It implies [`PER_LAUNCH_CLAIMS`].
pub const PER_INSTANCE_CLAIMS: &str = "per_instance";

/// SPEC §§9.1, 13.1: what `begin_residency` found for a Park or Restore.
pub enum ResidencyStart {
    /// Intent is durable before any effect: the named launch is owned, its
    /// journaled group is alive and unchanged, and it is in the state this
    /// action changes (`ready` for Park, `parked` for Restore).
    Begun {
        owner: Box<MemberCommand>,
        expected: Vec<ProcessIdentity>,
    },
    /// The launch is not in a state this action may change, or its group is
    /// not the journaled one. Nothing was done; the command completed with
    /// `unchanged` evidence.
    Refused,
}

/// How a begun Park or Restore ended. Each is persisted with its evidence
/// before the result is sent (SPEC §13.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidencyOutcome {
    /// Refused before any engine call; the launch keeps its prior state.
    Unchanged,
    /// Parked in place, group unchanged.
    Parked,
    /// Restored and proven usable by a fresh model probe (SPEC §6.1).
    Restored,
    /// An engine effect may have happened and its outcome is unknown. The
    /// launch is quarantined: only a Terminate settles it (SPEC §13.2, T20).
    Uncertain,
}

/// A retained launch's durable residency, absent while it is resident.
const RESIDENCY_STATES: [&str; 4] = ["parking", "parked", "restoring", "uncertain"];

pub struct HostJournal {
    directory: IdentityDirectory,
    path: PathBuf,
    database_device: u64,
    database_inode: u64,
    db: Mutex<Connection>,
    /// Serializes session loss against release of the native launch gate.
    transition: Mutex<()>,
    connected_session: AtomicU64,
    controller: String,
    host: String,
}
impl HostJournal {
    /// `path` must be an explicitly created private journal directory, separate
    /// from the enrollment identity directory. It is never repaired on startup.
    pub fn open(path: &Path, controller: &str, host: &str) -> Result<Arc<Self>, JournalError> {
        if [controller, host]
            .iter()
            .any(|v| v.is_empty() || v.len() > 256)
        {
            return Err(JournalError::Conflict);
        }
        let directory = IdentityDirectory::open(path).map_err(|error| match error {
            crate::identity_storage::StorageError::Busy => JournalError::Busy,
            _ => JournalError::Storage,
        })?;
        validate_files(path)?;
        let database = path.join("commands.sqlite");
        let exists = database.try_exists().map_err(|_| JournalError::Storage)?;
        // SPEC §13.2 / T33: the marker says the journal is established. It is
        // written only after the schema is durable, so an interrupted first
        // open leaves either nothing or a pristine, never-used database that
        // the next open completes. A database missing under its marker was
        // established and is never reinitialized.
        let marked = match directory
            .read_bundle(MARKER)
            .map_err(|_| JournalError::Storage)?
            .as_deref()
        {
            None => false,
            Some(MARKER_BYTES) => true,
            Some(_) => return Err(JournalError::Storage),
        };
        if !exists && marked {
            return Err(JournalError::Storage);
        }
        let leaf = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(!exists)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&database)
            .map_err(|_| JournalError::Storage)?;
        let database_metadata = leaf.metadata().map_err(|_| JournalError::Storage)?;
        leaf.sync_all().map_err(|_| JournalError::Storage)?;
        fs::File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(|_| JournalError::Storage)?;
        validate_files(path)?;
        let mut db = Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table'",
            [],
            |r| r.get(0),
        )?;
        // Unmarked: only an interrupted initialization is adopted. Pristine (no
        // schema) is initialized now; a complete schema that never held a
        // command nor a session only lacks its marker.
        let fresh = !marked && version == 0 && tables == 0;
        // SPEC §13.2 / T33: refuse a newer schema before anything is written.
        if version > JOURNAL_SCHEMA_VERSION {
            return Err(JournalError::FromNewerVersion {
                found: version,
                supported: JOURNAL_SCHEMA_VERSION,
            });
        }
        if !marked && !fresh {
            let untouched = version == JOURNAL_SCHEMA_VERSION
                && db
                    .query_row("SELECT count(*) FROM commands", [], |r| r.get::<_, i64>(0))
                    .ok()
                    == Some(0)
                && db
                    .query_row("SELECT session FROM authority WHERE singleton=1", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .ok()
                    == Some(0);
            if !untouched {
                return Err(JournalError::Storage);
            }
        }
        if !fresh && !(1..=JOURNAL_SCHEMA_VERSION).contains(&version) {
            return Err(JournalError::Storage);
        }
        let tx = db.transaction()?;
        if fresh {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS authority(singleton INTEGER PRIMARY KEY CHECK(singleton=1),controller TEXT NOT NULL,host TEXT NOT NULL,session INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS assignments(deployment TEXT PRIMARY KEY,generation INTEGER NOT NULL,revision INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS commands(sequence INTEGER PRIMARY KEY AUTOINCREMENT,command_id TEXT NOT NULL UNIQUE,digest BLOB NOT NULL,body BLOB,deployment TEXT NOT NULL,member TEXT NOT NULL,generation INTEGER NOT NULL,revision INTEGER NOT NULL,operation TEXT NOT NULL,step TEXT NOT NULL,state INTEGER NOT NULL,claim INTEGER NOT NULL CHECK(claim IN(0,1)), UNIQUE(deployment,generation,operation,step));
          CREATE UNIQUE INDEX IF NOT EXISTS one_host_claim ON commands(claim) WHERE claim=1;
          CREATE TABLE IF NOT EXISTS processes(command_id TEXT NOT NULL REFERENCES commands(command_id),role TEXT NOT NULL,pid INTEGER NOT NULL,boot TEXT NOT NULL,ticks INTEGER NOT NULL, PRIMARY KEY(command_id,pid,boot,ticks)); PRAGMA user_version=1;")?;
            tx.execute(
                "INSERT OR IGNORE INTO authority VALUES(1,?1,?2,0)",
                params![controller, host],
            )?;
        }
        // Missing schema is corrupt state, never an invitation to reset ownership.
        let tables: i64 = tx.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('authority','assignments','commands','processes')", [], |r|r.get(0))?;
        if tables != 4 {
            return Err(JournalError::Storage);
        }
        if version < 2 {
            tx.execute_batch("CREATE TABLE native_results(command_id TEXT PRIMARY KEY REFERENCES commands(command_id), result BLOB NOT NULL); PRAGMA user_version=2;")?;
        } else if tx.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='native_results'",
            [],
            |r| r.get::<_, i64>(0),
        )? != 1
        {
            return Err(JournalError::Storage);
        }
        // SPEC §§9.1, 13.1 (W4): one durable residency row per parked, parking,
        // restoring or quarantined launch; a resident launch has none.
        if version < 3 {
            tx.execute_batch("CREATE TABLE residency(owner TEXT PRIMARY KEY REFERENCES commands(command_id), state TEXT NOT NULL CHECK(state IN ('parking','parked','restoring','uncertain')), command_id TEXT NOT NULL); PRAGMA user_version=3;")?;
        } else if tx.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='residency'",
            [],
            |r| r.get::<_, i64>(0),
        )? != 1
        {
            return Err(JournalError::Storage);
        }
        // SPEC §§3.1, 7.3, 13.1 (per-launch claims): the host-wide single
        // claim becomes one claim per instance incarnation, keyed by
        // (deployment, generation) and owned handle (the command id). A
        // journal from before holds at most one claim, so every retained
        // launch is adopted as its own claim unchanged.
        let index = |name: &str| -> Result<i64, JournalError> {
            Ok(tx.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name=?1",
                [name],
                |r| r.get(0),
            )?)
        };
        if version < 4 {
            tx.execute_batch("DROP INDEX IF EXISTS one_host_claim; CREATE UNIQUE INDEX IF NOT EXISTS one_claim_per_instance ON commands(deployment,generation) WHERE claim=1; PRAGMA user_version=4;")?;
        } else if index("one_claim_per_instance")? != 1 || index("one_host_claim")? != 0 {
            return Err(JournalError::Storage);
        }
        // ADR 0013 §5 (per-instance fencing, v5): a deployment's instances
        // draw generations from one counter, so fencing every instance by the
        // deployment's highest generation refused an older instance still
        // running beside a newer one. Assignments are keyed by (deployment,
        // instance), and each command records the instance it is fenced as.
        // Every assignment and command journaled before is instance 0, which
        // is what every earlier command's identity decodes as.
        let column = |table: &str| -> Result<i64, JournalError> {
            Ok(tx.query_row(
                "SELECT count(*) FROM pragma_table_info(?1) WHERE name='instance'",
                [table],
                |r| r.get(0),
            )?)
        };
        if version < 5 {
            tx.execute_batch("CREATE TABLE assignments_v5(deployment TEXT NOT NULL,instance INTEGER NOT NULL CHECK(instance>=0),generation INTEGER NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(deployment,instance));
              INSERT INTO assignments_v5(deployment,instance,generation,revision) SELECT deployment,0,generation,revision FROM assignments;
              DROP TABLE assignments; ALTER TABLE assignments_v5 RENAME TO assignments;
              ALTER TABLE commands ADD COLUMN instance INTEGER NOT NULL DEFAULT 0 CHECK(instance>=0);
              PRAGMA user_version=5;")?;
        } else if column("assignments")? != 1 || column("commands")? != 1 {
            return Err(JournalError::Storage);
        }
        // SPEC §13.2 (W13): the first observed exit of each owned process of a
        // Ready launch, so a report after an agent restart still says how the
        // engine ended. Evidence only: it grants and releases nothing, and a
        // journal without it (an older build) is the same schema version.
        // ADR 0014 §7 (WE3), owner decision 5: the checkpoint digest this host
        // measured when it parked a launch journaled before WE3 (whose plan
        // records none), so its wake verifies against the host's own
        // measurement. Evidence only, same schema version like member_exits.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS park_digests(owner TEXT PRIMARY KEY,digest TEXT NOT NULL);",
        )?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS member_exits(command_id TEXT NOT NULL REFERENCES commands(command_id),role TEXT NOT NULL,pid INTEGER NOT NULL,boot TEXT NOT NULL,ticks INTEGER NOT NULL,code INTEGER,signal INTEGER,observed_at INTEGER NOT NULL,PRIMARY KEY(command_id,pid,boot,ticks));")?;
        // ADR 0028 §11: the Terminates whose recorded group outlived SIGTERM
        // and its bounded wait and was ended with SIGKILL. Evidence only, same
        // schema version like member_exits.
        tx.execute_batch("CREATE TABLE IF NOT EXISTS terminate_escalations(command_id TEXT PRIMARY KEY REFERENCES commands(command_id));")?;
        let bound: (String, String) = tx.query_row(
            "SELECT controller,host FROM authority WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if bound != (controller.into(), host.into()) {
            return Err(JournalError::Conflict);
        }
        tx.commit()?;
        if !marked {
            // The schema is durable (synchronous=FULL, rollback journal); only
            // now is the journal established.
            fs::File::open(path)
                .and_then(|f| f.sync_all())
                .map_err(|_| JournalError::Storage)?;
            directory
                .create_bundle(MARKER, MARKER_BYTES)
                .map_err(|_| JournalError::Storage)?;
        }
        Ok(Arc::new(Self {
            directory,
            path: path.into(),
            database_device: database_metadata.dev(),
            database_inode: database_metadata.ino(),
            db: Mutex::new(db),
            transition: Mutex::new(()),
            connected_session: AtomicU64::new(0),
            controller: controller.into(),
            host: host.into(),
        }))
    }
    fn validate(&self) -> Result<(), JournalError> {
        self.directory
            .validate()
            .map_err(|_| JournalError::Storage)?;
        validate_files(&self.path)?;
        let metadata = fs::symlink_metadata(self.path.join("commands.sqlite"))
            .map_err(|_| JournalError::Storage)?;
        if metadata.dev() != self.database_device
            || metadata.ino() != self.database_inode
            || self
                .directory
                .read_bundle(MARKER)
                .map_err(|_| JournalError::Storage)?
                .as_deref()
                != Some(MARKER_BYTES)
        {
            return Err(JournalError::Storage);
        }
        Ok(())
    }
    /// Whether neither of the journal's locks is held at this moment. SPEC
    /// §13.2: slow work (a GPU collector run, checkpoint hashing) runs outside
    /// them; tests observe that from the slow work itself. Not an admission
    /// input: the answer is stale as soon as it is returned.
    #[doc(hidden)]
    pub fn locks_free(&self) -> bool {
        self.transition.try_lock().is_ok() && self.db.try_lock().is_ok()
    }
    /// Call only after authenticated enrollment/session authorization and remote
    /// reconciliation. Every new stream fences all prior stream capabilities.
    pub fn connect(&self) -> Result<u64, JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let session:i64 = db.query_row("UPDATE authority SET session=session+1 WHERE singleton=1 AND session<9223372036854775807 RETURNING session",[],|r|r.get(0))?;
        self.connected_session
            .store(session as u64, Ordering::SeqCst);
        Ok(session as u64)
    }
    pub fn disconnect(&self, session: u64) -> Result<(), JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.connected_session
            .compare_exchange(session, 0, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| JournalError::Fenced)?;
        Ok(())
    }
    fn check_session(&self, session: u64) -> Result<(), JournalError> {
        if session == 0 || self.connected_session.load(Ordering::SeqCst) != session {
            Err(JournalError::Fenced)
        } else {
            Ok(())
        }
    }
    pub fn accept(
        self: &Arc<Self>,
        session: u64,
        command: &MemberCommand,
        now_ms: i64,
        policy: &dyn LocalExecutionPolicy,
    ) -> Result<Acceptance, JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        self.check_session(session)?;
        command
            .verify_digest()
            .map_err(|_| JournalError::Conflict)?;
        let id = &command.identity;
        if id.controller_id != self.controller || id.member.host_id != self.host {
            return Err(JournalError::Fenced);
        }
        let body = command.to_wire().encode_to_vec();
        if body.len() > MAX_COMMAND_BYTES {
            return Err(JournalError::Conflict);
        }
        let mut db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let tx = db.transaction()?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT digest FROM commands WHERE command_id=?1",
                [&id.command_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(digest) = existing {
            if digest != id.payload_digest {
                return Err(JournalError::Conflict);
            }
            // Replays may return evidence after expiry, but never effect authority.
            return Ok(Acceptance::Replay(record(&tx, &id.command_id)?));
        }
        if now_ms < 0 || now_ms >= id.deadline_ms {
            return Err(JournalError::Fenced);
        }
        policy.authorize(command)?;
        if !matches!(
            command.action,
            MemberAction::Launch { .. }
                | MemberAction::LaunchSingle(_)
                | MemberAction::Inspect
                | MemberAction::Terminate { .. }
                | MemberAction::Probe { .. }
                | MemberAction::Park { .. }
                | MemberAction::Restore { .. }
        ) {
            return Err(JournalError::Unauthorized);
        }
        if matches!(
            command.action,
            MemberAction::Park { .. } | MemberAction::Restore { .. }
        ) {
            // SPEC §§9.1, 10: a residency change names a launch this host owns,
            // at its declared tier, in the state the action changes. Anything
            // else is refused before it is journaled.
            let owner = residency_owner(&tx, command)?;
            policy.authorize_residency(command, &owner)?;
            residency_precondition(&tx, command)?;
            if matches!(command.action, MemberAction::Restore { .. }) {
                // SPEC §§3.1, 7.3, 9.1: a wake grows the launch from its
                // parked to its ready footprint; it must still fit beside
                // every other launch this host claims, or it stays parked.
                let others = claimed_launches(&tx, &owner.identity.command_id)?;
                policy.admit_wake(command, &owner, &others)?;
            }
        }
        // ADR 0013 §5: fenced by this instance's own last assignment only.
        let instance = fence_instance(&tx, command)?;
        let assignment: Option<(i64, i64)> = tx
            .query_row(
                "SELECT generation,revision FROM assignments WHERE deployment=?1 AND instance=?2",
                params![id.deployment_id, instance],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if assignment.is_some_and(|(g, r)| id.generation < g || id.revision < r) {
            return Err(JournalError::Fenced);
        }
        let claim = matches!(
            command.action,
            MemberAction::Launch { .. } | MemberAction::LaunchSingle(_)
        );
        if claim {
            // SPEC §§3.1, 7.3: one claim per instance incarnation; a second
            // launch of the same (deployment, generation) stays uncertain.
            // Any other launch is admitted only beside every retained claim,
            // against the host's own policy.
            let claimed = claimed_launches(&tx, &id.command_id)?;
            if claimed.iter().any(|c| {
                c.command.identity.deployment_id == id.deployment_id
                    && c.command.identity.generation == id.generation
            }) {
                return Err(JournalError::Uncertain);
            }
            policy.admit_beside(command, &claimed)?;
        }
        let duplicate_step:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM commands WHERE deployment=?1 AND generation=?2 AND operation=?3 AND step=?4)",params![id.deployment_id,id.generation,id.operation_id,id.step_id],|r|r.get(0))?;
        if duplicate_step {
            return Err(JournalError::Conflict);
        }
        tx.execute("INSERT INTO commands(command_id,digest,body,deployment,member,generation,revision,operation,step,state,claim,instance) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,0,?10,?11)",params![id.command_id,id.payload_digest.as_slice(),body,id.deployment_id,id.member.member_id,id.generation,id.revision,id.operation_id,id.step_id,claim,instance])?;
        tx.execute("INSERT INTO assignments(deployment,instance,generation,revision) VALUES(?1,?2,?3,?4) ON CONFLICT(deployment,instance) DO UPDATE SET generation=excluded.generation,revision=excluded.revision",params![id.deployment_id,instance,id.generation,id.revision])?;
        tx.commit()?;
        Ok(Acceptance::Fresh(ExecutionTicket {
            journal: Arc::downgrade(self),
            command_id: id.command_id.clone(),
            session,
        }))
    }
    /// ADR 0014 §7 (WE3): journal the digest this host measured for a parked
    /// pre-WE3 launch (owner decision 5). Replaces any earlier one.
    pub fn record_park_digest(&self, owner: &str, digest: &str) -> Result<(), JournalError> {
        self.validate()?;
        if digest.is_empty() || digest.len() > 128 {
            return Err(JournalError::Conflict);
        }
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        db.execute(
            "INSERT INTO park_digests(owner,digest) VALUES(?1,?2) ON CONFLICT(owner) DO UPDATE SET digest=excluded.digest",
            params![owner, digest],
        )?;
        Ok(())
    }

    /// The digest journaled when `owner` was last parked, if any.
    pub fn park_digest(&self, owner: &str) -> Result<Option<String>, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        Ok(db
            .query_row(
                "SELECT digest FROM park_digests WHERE owner=?1",
                [owner],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// SPEC §13.2: the cheap fence and duplicate checks `accept` makes, read
    /// without the transition lock, so the slow half of admission runs only
    /// for a command that could be accepted. `Ok(false)` is a replay of a
    /// journaled command; an error is what `accept` would refuse with. It
    /// grants nothing: `accept` checks all of it again under its lock.
    pub fn precheck(&self, session: u64, command: &MemberCommand) -> Result<bool, JournalError> {
        self.validate()?;
        self.check_session(session)?;
        let id = &command.identity;
        if id.controller_id != self.controller || id.member.host_id != self.host {
            return Err(JournalError::Fenced);
        }
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let existing: Option<Vec<u8>> = db
            .query_row(
                "SELECT digest FROM commands WHERE command_id=?1",
                [&id.command_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(digest) = existing {
            return if digest == id.payload_digest {
                Ok(false)
            } else {
                Err(JournalError::Conflict)
            };
        }
        let instance = fence_instance(&db, command)?;
        let assignment: Option<(i64, i64)> = db
            .query_row(
                "SELECT generation,revision FROM assignments WHERE deployment=?1 AND instance=?2",
                params![id.deployment_id, instance],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if assignment.is_some_and(|(g, r)| id.generation < g || id.revision < r) {
            return Err(JournalError::Fenced);
        }
        Ok(true)
    }

    /// SPEC §§3.1, 7.3: every launch claim this host retains, except the
    /// command named `except` (a launch being admitted is never its own
    /// neighbour). Evidence for local admission only, never authority.
    pub fn claimed_launches(&self, except: &str) -> Result<Vec<ClaimedLaunch>, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        claimed_launches(&db, except)
    }
    /// ADR 0023 §3: run `f` over every process recorded for a launch this host
    /// still claims, holding the journal so no launch records a process (and
    /// so starts) until `f` returns. A claimed launch with no recorded process
    /// has started nothing: the launcher holds a child at its gate until the
    /// child is recorded.
    pub fn with_claimed_processes<T>(
        &self,
        f: impl FnOnce(&[ProcessIdentity]) -> T,
    ) -> Result<T, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let mut processes = Vec::new();
        for claimed in claimed_launches(&db, "")? {
            processes.extend(record(&db, &claimed.command.identity.command_id)?.processes);
        }
        Ok(f(&processes))
    }
    /// Read a retained immutable request, never a new execution capability.
    pub fn retained_command(&self, command_id: &str) -> Result<MemberCommand, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        load_command(&db, command_id)
    }
    pub fn history(
        &self,
        after_sequence: i64,
        limit: usize,
    ) -> Result<Vec<CommandRecord>, JournalError> {
        self.validate()?;
        if after_sequence < 0 || limit == 0 || limit > MAX_HISTORY {
            return Err(JournalError::Conflict);
        }
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let ids = db
            .prepare(
                "SELECT command_id FROM commands WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
            )?
            .query_map(params![after_sequence, limit], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut records = Vec::new();
        let mut bytes = 0;
        for id in ids {
            let record = record(&db, &id)?;
            bytes += record.command_id.len()
                + 128
                + record
                    .processes
                    .iter()
                    .map(|p| p.role.len() + p.boot_id.len() + 128)
                    .sum::<usize>();
            if bytes > MAX_HISTORY_BYTES {
                break;
            }
            records.push(record);
        }
        Ok(records)
    }
    /// Payload compaction retains identity/digest tombstones forever. Claims and
    /// unresolved results are never compacted. Pagination bounds resume replies.
    pub fn compact_completed(&self, through_sequence: i64) -> Result<usize, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        Ok(db.execute(
            "UPDATE commands SET body=NULL,state=4 WHERE sequence<=?1 AND state=3 AND claim=0",
            [through_sequence],
        )?)
    }
    pub fn execute(
        self: &Arc<Self>,
        ticket: ExecutionTicket,
        now_ms: i64,
        policy: &dyn LocalExecutionPolicy,
    ) -> Result<CommandRecord, JournalError> {
        self.execute_with_clock(
            ticket,
            Arc::new(AnchoredClock {
                epoch_ms: now_ms,
                started: Instant::now(),
            }),
            policy,
        )
    }
    pub fn execute_with_clock(
        self: &Arc<Self>,
        ticket: ExecutionTicket,
        clock: Arc<dyn ExecutionClock>,
        policy: &dyn LocalExecutionPolicy,
    ) -> Result<CommandRecord, JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        self.check_session(ticket.session)?;
        if !Weak::ptr_eq(&ticket.journal, &Arc::downgrade(self)) {
            return Err(JournalError::Fenced);
        }
        let command = self.begin_attempt(&ticket, clock.as_ref(), policy)?;
        let tools = Arc::new(DurableProcessLaunch::new(Arc::new(JournalAssociation {
            journal: Arc::downgrade(self),
            command_id: ticket.command_id.clone(),
            effect: Some((
                Arc::clone(&clock),
                command.identity.deadline_ms,
                ticket.session,
            )),
        })));
        match &command.action {
            MemberAction::Launch { .. } | MemberAction::LaunchSingle(_) => {
                let rendered = policy.render_launch(&command)?;
                before_deadline(clock.as_ref(), command.identity.deadline_ms)?;
                self.check_session(ticket.session)?;
                match rendered.descriptors.as_ref() {
                    Some(descriptors) => tools.spawn_durable_protected(
                        &ticket.command_id,
                        &rendered.command,
                        descriptors,
                    ),
                    None => tools.spawn_durable(&ticket.command_id, &rendered.command),
                }
                .map_err(|_| JournalError::Uncertain)?;
                let db = self.db.lock().map_err(|_| JournalError::Storage)?;
                db.execute(
                    "UPDATE commands SET state=2 WHERE command_id=?1 AND state=1",
                    [&ticket.command_id],
                )?;
            }
            MemberAction::Inspect => {
                // Inspection never manufactures cleanup or releases a claim.
                self.refresh_owned(&tools)?;
                self.complete(&ticket.command_id)?;
            }
            MemberAction::Terminate { owned_handle, .. } => {
                // Every branch below runs under the transition lock taken above,
                // which is also held across any spawn of this owner's launch
                // tools. A spawn therefore either finished (and journaled its
                // identity) before this reads the owner, or observes the released
                // claim afterwards and refuses to start anything.
                let identities = match self.terminate_target(&command, owned_handle)? {
                    TerminateTarget::Owned(identities) => identities,
                    TerminateTarget::Unknown => {
                        // SPEC §13.1 / T34: this host never accepted the launch.
                        // That is authenticated evidence nothing of it runs here,
                        // and the handle is fenced permanently so a delivery still
                        // in flight can never be accepted and start it afterwards.
                        before_deadline(clock.as_ref(), command.identity.deadline_ms)?;
                        self.check_session(ticket.session)?;
                        let mut db = self.db.lock().map_err(|_| JournalError::Storage)?;
                        let tx = db.transaction()?;
                        let id = &command.identity;
                        tx.execute("INSERT INTO commands(command_id,digest,body,deployment,member,generation,revision,operation,step,state,claim,instance) VALUES(?1,?2,NULL,?3,?4,?5,?6,?7,?8,4,0,?9)",
                            params![owned_handle, [0u8; 32].as_slice(), id.deployment_id, id.member.member_id, id.generation, id.revision, format!("fenced:{}", id.command_id), owned_handle, id.instance_index])?;
                        tx.execute(
                            "UPDATE commands SET state=3 WHERE command_id=?1",
                            [&ticket.command_id],
                        )?;
                        tx.commit()?;
                        return record(&db, &ticket.command_id);
                    }
                };
                before_deadline(clock.as_ref(), command.identity.deadline_ms)?;
                self.check_session(ticket.session)?;
                // ADR 0028 §11: SIGTERM, a bounded wait, then SIGKILL; whether
                // SIGKILL was needed is journaled with this Terminate.
                let escalated = if identities.is_empty() {
                    false
                } else {
                    tools
                        .terminate_escalating(&identities, Duration::from_secs(2))
                        .map_err(|_| JournalError::Uncertain)?
                };
                let mut db = self.db.lock().map_err(|_| JournalError::Storage)?;
                let tx = db.transaction()?;
                // A compacted tombstone keeps its tombstone state; its claim was
                // already released when it completed.
                tx.execute(
                    "UPDATE commands SET state=CASE WHEN state=4 THEN 4 ELSE 3 END,claim=0 WHERE command_id=?1",
                    [owned_handle],
                )?;
                // A launch proven gone has no residency left to track (W4).
                tx.execute("DELETE FROM residency WHERE owner=?1", [owned_handle])?;
                if escalated {
                    tx.execute(
                        "INSERT OR IGNORE INTO terminate_escalations(command_id) VALUES(?1)",
                        [&ticket.command_id],
                    )?;
                }
                tx.execute(
                    "UPDATE commands SET state=3 WHERE command_id=?1",
                    [&ticket.command_id],
                )?;
                tx.commit()?;
            }
            MemberAction::Probe { .. } => {
                // SPEC §§6.1, 13.2: a probe has no effect of its own. The native
                // adapter probes only after `probe_target` binds it to one owned
                // retained launch of the same member, deployment and profile; a
                // probe naming anything else completes having proven nothing.
                self.complete(&ticket.command_id)?;
            }
            _ => return Err(JournalError::Unauthorized),
        }
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        record(&db, &ticket.command_id)
    }
    /// What an accepted Terminate may act on. Caller holds the transition lock.
    fn terminate_target(
        &self,
        command: &MemberCommand,
        owned_handle: &str,
    ) -> Result<TerminateTarget, JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let body: Option<Option<Vec<u8>>> = db
            .query_row(
                "SELECT body FROM commands WHERE command_id=?1",
                [owned_handle],
                |r| r.get(0),
            )
            .optional()?;
        let owner_record = match body {
            None => return Ok(TerminateTarget::Unknown),
            // Compaction keeps identities and releases nothing; a compacted
            // command never retained a claim. SPEC §13.1 / T34: its owner is
            // still the deployment and member its row names, so a Terminate
            // from anyone else is refused and learns nothing of it.
            Some(None) => {
                let (deployment, member): (String, String) = db.query_row(
                    "SELECT deployment,member FROM commands WHERE command_id=?1",
                    [owned_handle],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if deployment != command.identity.deployment_id
                    || member != command.identity.member.member_id
                {
                    return Err(JournalError::Unauthorized);
                }
                let owner_record = record(&db, owned_handle)?;
                if owner_record.claim_retained {
                    return Err(JournalError::Storage);
                }
                owner_record
            }
            Some(Some(body)) => {
                let owner = decode(&body)?;
                if !same_owner(&owner, command)
                    || !matches!(
                        owner.action,
                        MemberAction::Launch { .. } | MemberAction::LaunchSingle(_)
                    )
                {
                    return Err(JournalError::Unauthorized);
                }
                record(&db, owned_handle)?
            }
        };
        // SPEC §13.2: a durable launch releases its gated child only after the
        // child's identity is journaled, and a gated child whose launcher dies
        // reads EOF and exits without executing the engine. So an owner with no
        // journaled process that never reached `launched`, or whose claim is
        // already released, owns nothing that could be running. Anything else
        // without identities is unexplained and stays retained.
        if owner_record.processes.is_empty()
            && owner_record.claim_retained
            && !matches!(
                owner_record.state,
                CommandState::Accepted | CommandState::Attempted
            )
        {
            return Err(JournalError::Uncertain);
        }
        Ok(TerminateTarget::Owned(owner_record.processes))
    }
    fn begin_attempt(
        &self,
        ticket: &ExecutionTicket,
        clock: &dyn ExecutionClock,
        policy: &dyn LocalExecutionPolicy,
    ) -> Result<MemberCommand, JournalError> {
        let mut db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let tx = db.transaction()?;
        let (body, state, instance): (Vec<u8>, i64, i64) = tx.query_row(
            "SELECT body,state,instance FROM commands WHERE command_id=?1",
            [&ticket.command_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if state != 0 {
            return Err(JournalError::Uncertain);
        }
        let command = decode(&body)?;
        let id = &command.identity;
        // ADR 0013 §5: the instance this command was fenced as at accept.
        let assignment: (i64, i64) = tx.query_row(
            "SELECT generation,revision FROM assignments WHERE deployment=?1 AND instance=?2",
            params![id.deployment_id, instance],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        before_deadline(clock, id.deadline_ms)?;
        if assignment != (id.generation, id.revision) {
            return Err(JournalError::Fenced);
        }
        policy.authorize(&command)?;
        if matches!(
            command.action,
            MemberAction::Launch { .. } | MemberAction::LaunchSingle(_)
        ) {
            // Defense in depth: the launch still fits beside every other
            // claim this host retains, just before its durable attempt.
            policy.admit_beside(&command, &claimed_launches(&tx, &ticket.command_id)?)?;
        }
        before_deadline(clock, id.deadline_ms)?;
        // SPEC §13.1: this commit precedes any process or ingress effect.
        tx.execute(
            "UPDATE commands SET state=1 WHERE command_id=?1",
            [&ticket.command_id],
        )?;
        tx.commit()?;
        Ok(command)
    }

    /// SPEC §13: a fresh durable ticket supplies one local native adapter with
    /// gated process tools. The adapter remains responsible for model readiness;
    /// these tools confer neither completion nor cleanup authority.
    pub fn launch_tools(
        self: &Arc<Self>,
        ticket: ExecutionTicket,
        now_ms: i64,
        policy: Arc<dyn LocalExecutionPolicy + Send + Sync>,
    ) -> Result<Arc<dyn OwnedProcessLaunch>, JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        self.check_session(ticket.session)?;
        if !Weak::ptr_eq(&ticket.journal, &Arc::downgrade(self)) {
            return Err(JournalError::Fenced);
        }
        let clock: Arc<dyn ExecutionClock> = Arc::new(AnchoredClock {
            epoch_ms: now_ms,
            started: Instant::now(),
        });
        let command = self.begin_attempt(&ticket, clock.as_ref(), policy.as_ref())?;
        if !matches!(
            command.action,
            MemberAction::Launch { .. } | MemberAction::LaunchSingle(_)
        ) {
            return Err(JournalError::Unauthorized);
        }
        let incarnation = match command.action.launch_plan() {
            Some(plan) => plan.incarnation.clone(),
            None => ticket.command_id.clone(),
        };
        let tools = DurableProcessLaunch::new(Arc::new(JournalAssociation {
            journal: Arc::downgrade(self),
            command_id: ticket.command_id.clone(),
            effect: Some((clock.clone(), command.identity.deadline_ms, ticket.session)),
        }));
        Ok(Arc::new(JournalLaunchTools {
            journal: self.clone(),
            command,
            session: ticket.session,
            clock,
            tools,
            incarnation,
            policy,
            spent: AtomicBool::new(false),
        }))
    }

    fn complete(&self, id: &str) -> Result<(), JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        db.execute(
            "UPDATE commands SET state=3 WHERE command_id=?1 AND claim=0",
            [id],
        )?;
        Ok(())
    }
    fn refresh_owned(&self, tools: &DurableProcessLaunch) -> Result<(), JournalError> {
        let owners = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            let mut query = db.prepare("SELECT command_id FROM commands WHERE claim=1")?;
            let ids = query
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };
        // SPEC §§3.1, 13.2 (per-launch claims): uncertainty is per launch. A
        // launch whose leader is gone or whose group cannot be read is left as
        // journaled and makes the inspection uncertain, but every other
        // claimed launch is still refreshed.
        let mut uncertain = false;
        for owner in owners {
            let processes = {
                let db = self.db.lock().map_err(|_| JournalError::Storage)?;
                record(&db, &owner)?.processes
            };
            let Some(api) = processes.iter().find(|p| p.role == "api") else {
                uncertain = true;
                continue;
            };
            // A reused leader PID cannot authorize adoption of a new group.
            if tools.present(api) != Presence::Alive {
                uncertain = true;
                continue;
            }
            let Ok(observed) = tools.observe_group(api) else {
                uncertain = true;
                continue;
            };
            let mut db = self.db.lock().map_err(|_| JournalError::Storage)?;
            let ready: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM native_results WHERE command_id=?1)",
                [&owner],
                |r| r.get(0),
            )?;
            let tx = db.transaction()?;
            for identity in after_readiness(&processes, observed, ready) {
                persist_process(&tx, &owner, &identity)?;
            }
            tx.commit()?;
        }
        if uncertain {
            return Err(JournalError::Uncertain);
        }
        Ok(())
    }
    /// SPEC §6.1: only the native adapter's complete model probe evidence can
    /// mark a launch usable. Persist before sending the result over transport.
    /// SPEC §13.2: written under the transition lock, for the session still
    /// connected, so a concurrent Terminate or a newer session is never
    /// overtaken by a late readiness write.
    pub fn record_launch_ready(
        self: &Arc<Self>,
        session: u64,
        command_id: &str,
        observation: &capyctl_domain::completion::EffectObservation,
    ) -> Result<(), JournalError> {
        use capyctl_domain::completion::Milestone;
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        self.check_session(session)?;
        let command = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            load_command(&db, command_id)?
        };
        let Some(plan) = command.action.launch_plan() else {
            return Err(JournalError::Unauthorized);
        };
        let identity = &command.identity;
        let token = &observation.token;
        if observation.binding_id != plan.binding_id
            || observation.incarnation != plan.incarnation
            || token.deployment_id != identity.deployment_id
            || token.operation_id != identity.operation_id
            || token.step_id != identity.step_id
            || token.revision != identity.revision
            || token.generation != identity.generation
            || observation.observed_at_ms < plan.issued_at_ms
            || observation.observed_at_ms >= identity.deadline_ms
            || observation.receipt.is_empty()
            || observation.facts
                != [
                    Milestone::AllocationsRestored,
                    Milestone::WeightsUsable,
                    Milestone::CacheValid,
                    Milestone::ModelUsable,
                ]
        {
            return Err(JournalError::Conflict);
        }
        capyctl_domain::group::validate_local_processes(&observation.identities)
            .map_err(|_| JournalError::Conflict)?;
        let mut result = self.execution_result(command_id, observation.observed_at_ms)?;
        // ADR 0027: the observation names the group, helpers included. A
        // helper that has exited since is no reason to refuse it; an engine
        // process gone, or anything alive the observation does not name, is.
        if !capyctl_domain::completion::same_engine(
            &observation.identities,
            &alive_identities(&result),
        ) {
            return Err(JournalError::Uncertain);
        }
        result.model_usable = true;
        result.observed_at_unix_ms = observation.observed_at_ms;
        // ADR 0014 amendment A12: the kernel builds the adapter saw, so the
        // controller leaves their samples out of the startup peak.
        result.kernel_builds = observation
            .kernel_builds
            .iter()
            .take(capyctl_protocol::execution::MAX_KERNEL_BUILDS)
            .map(|build| capyctl_protocol::pb::KernelBuildSpan {
                from_unix_ms: build.from_ms,
                until_unix_ms: build.until_ms,
            })
            .collect();
        capyctl_protocol::execution::validate_result(&command, &result)
            .map_err(|_| JournalError::Conflict)?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        db.execute("INSERT INTO native_results(command_id,result) VALUES(?1,?2) ON CONFLICT(command_id) DO UPDATE SET result=excluded.result", params![command_id, result.encode_to_vec()])?;
        Ok(())
    }

    /// ADR 0028 §8 (R23): the group Launch this host journaled for
    /// `member_id` of `deployment`'s instance `instance` at `generation`, if
    /// any: the member launch's key. Evidence only, never authority.
    pub fn group_launch(
        &self,
        deployment: &str,
        instance: u32,
        generation: i64,
        member_id: &str,
    ) -> Result<Option<String>, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let ids = db
            .prepare("SELECT command_id FROM commands WHERE deployment=?1 AND instance=?2 AND generation=?3 AND member=?4 AND body IS NOT NULL ORDER BY sequence")?
            .query_map(params![deployment, instance, generation, member_id], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            if matches!(load_command(&db, &id)?.action, MemberAction::Launch { .. }) {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// SPEC §§6.1, 13.2 (G2): the group a fresh probe of `owned_handle` must find.
    ///
    /// The launch must still hold its claim, must have recorded native readiness
    /// evidence, and every process that evidence named must still be the same
    /// live process (PID, boot and start identity) with nothing else alive in
    /// the group. Anything else is uncertain and grants no probe at all.
    pub fn probe_target(
        self: &Arc<Self>,
        probe: &MemberCommand,
    ) -> Result<Vec<ProcessIdentity>, JournalError> {
        self.validate()?;
        let MemberAction::Probe { owned_handle } = &probe.action else {
            return Err(JournalError::Unauthorized);
        };
        let owned_handle = owned_handle.as_str();
        let (owner, record, saved) = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            let saved: Vec<u8> = db
                .query_row(
                    "SELECT result FROM native_results WHERE command_id=?1",
                    [owned_handle],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or(JournalError::Uncertain)?;
            // SPEC §§6.1, 9.1: a parked, parking, restoring or quarantined
            // launch is not probed back to Ready; only a Restore ends a park.
            if residency_row(&db, owned_handle)?.is_some() {
                return Err(JournalError::Uncertain);
            }
            (
                load_command(&db, owned_handle)?,
                record(&db, owned_handle)?,
                saved,
            )
        };
        if !same_owner(&owner, probe)
            || !matches!(owner.action, MemberAction::LaunchSingle(_))
            || !record.claim_retained
            || record.state != CommandState::Launched
        {
            return Err(JournalError::Uncertain);
        }
        let saved = pb::MemberExecutionResult::decode(saved.as_slice())
            .map_err(|_| JournalError::Storage)?;
        capyctl_protocol::execution::validate_result(&owner, &saved)
            .map_err(|_| JournalError::Storage)?;
        if !saved.model_usable {
            return Err(JournalError::Uncertain);
        }
        let expected = alive_identities(&saved);
        let current: Vec<_> = self
            .inspect_owned(owned_handle)?
            .into_iter()
            .filter(|(_, presence)| *presence == Presence::Alive)
            .map(|(identity, _)| identity)
            .collect();
        // ADR 0027: every engine process of the Ready group; a helper may be gone.
        if !capyctl_domain::completion::same_engine(&expected, &current) {
            return Err(JournalError::Uncertain);
        }
        Ok(expected)
    }

    /// Persist a fresh probe's readiness for exactly the group `probe_target`
    /// returned, observed alive again now. Written before the result is sent.
    /// SPEC §13.2: written under the transition lock, for the session still
    /// connected (see `record_launch_ready`).
    pub fn record_probe_ready(
        self: &Arc<Self>,
        session: u64,
        probe_id: &str,
        observed_at_ms: i64,
        expected: &[ProcessIdentity],
    ) -> Result<(), JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        self.check_session(session)?;
        let probe = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            load_command(&db, probe_id)?
        };
        if observed_at_ms >= probe.identity.deadline_ms
            || sorted(self.probe_target(&probe)?) != sorted(expected.to_vec())
        {
            return Err(JournalError::Uncertain);
        }
        let mut result = self.execution_result(probe_id, observed_at_ms)?;
        if !capyctl_domain::completion::same_engine(expected, &alive_identities(&result)) {
            return Err(JournalError::Uncertain);
        }
        result.model_usable = true;
        result.observed_at_unix_ms = observed_at_ms;
        capyctl_protocol::execution::validate_result(&probe, &result)
            .map_err(|_| JournalError::Conflict)?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        db.execute("INSERT INTO native_results(command_id,result) VALUES(?1,?2) ON CONFLICT(command_id) DO UPDATE SET result=excluded.result", params![probe_id, result.encode_to_vec()])?;
        Ok(())
    }

    /// SPEC §§9.1, 10, 13.1: start one accepted Park or Restore.
    ///
    /// The attempt and the intent (`parking` or `restoring`) are durable before
    /// any gate, engine or process effect. The launch it names must still hold
    /// its claim, must have recorded native readiness, and every process that
    /// readiness named must be the same live process (PID, boot and start
    /// identity) with nothing else alive in the group. A launch in any other
    /// state is refused without effect and the command completes `unchanged`.
    pub fn begin_residency(
        self: &Arc<Self>,
        ticket: ExecutionTicket,
        now_ms: i64,
        policy: &dyn LocalExecutionPolicy,
    ) -> Result<ResidencyStart, JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        self.check_session(ticket.session)?;
        if !Weak::ptr_eq(&ticket.journal, &Arc::downgrade(self)) {
            return Err(JournalError::Fenced);
        }
        let clock = AnchoredClock {
            epoch_ms: now_ms,
            started: Instant::now(),
        };
        let command = self.begin_attempt(&ticket, &clock, policy)?;
        let owner = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            residency_owner(&db, &command)?
        };
        // SPEC §§3.1, 7.3, 9.1: a wake is re-checked beside every other claim
        // just before its durable intent; a wake that no longer fits is
        // refused without effect and the launch stays parked.
        let wake_fits = || -> Result<(), JournalError> {
            if matches!(command.action, MemberAction::Restore { .. }) {
                let others = self.claimed_launches(&owner.identity.command_id)?;
                policy.admit_wake(&command, &owner, &others)?;
            }
            Ok(())
        };
        let expected = match policy
            .authorize_residency(&command, &owner)
            .and_then(|()| wake_fits())
        {
            Ok(()) => self.ready_group(&owner)?,
            Err(_) => None,
        };
        let begun = {
            let mut db = self.db.lock().map_err(|_| JournalError::Storage)?;
            let tx = db.transaction()?;
            let owned = owned_handle(&command)?;
            let begun = match (&expected, residency_precondition(&tx, &command)) {
                (Some(_), Ok(())) => {
                    match &command.action {
                        MemberAction::Park { .. } => tx.execute(
                            "INSERT INTO residency(owner,state,command_id) VALUES(?1,'parking',?2)",
                            params![owned, ticket.command_id],
                        )?,
                        _ => tx.execute(
                            "UPDATE residency SET state='restoring',command_id=?2 WHERE owner=?1 AND state='parked'",
                            params![owned, ticket.command_id],
                        )?,
                    };
                    true
                }
                _ => false,
            };
            tx.commit()?;
            begun
        };
        match (begun, expected) {
            (true, Some(expected)) => Ok(ResidencyStart::Begun {
                owner: Box::new(owner),
                expected,
            }),
            _ => {
                self.finish_locked(
                    &ticket.command_id,
                    ResidencyOutcome::Unchanged,
                    unknown_residency(),
                    &[],
                    clock.now_ms()?,
                )?;
                Ok(ResidencyStart::Refused)
            }
        }
    }

    /// The live group of `owner` when it is exactly the group its recorded
    /// native readiness named, and the launch still holds its claim.
    fn ready_group(
        self: &Arc<Self>,
        owner: &MemberCommand,
    ) -> Result<Option<Vec<ProcessIdentity>>, JournalError> {
        let id = &owner.identity.command_id;
        let (record, saved) = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            let saved: Option<Vec<u8>> = db
                .query_row(
                    "SELECT result FROM native_results WHERE command_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            (record(&db, id)?, saved)
        };
        let Some(saved) = saved else { return Ok(None) };
        let saved = pb::MemberExecutionResult::decode(saved.as_slice())
            .map_err(|_| JournalError::Storage)?;
        capyctl_protocol::execution::validate_result(owner, &saved)
            .map_err(|_| JournalError::Storage)?;
        if !record.claim_retained || record.state != CommandState::Launched || !saved.model_usable {
            return Ok(None);
        }
        let expected = alive_identities(&saved);
        let current: Vec<_> = self
            .inspect_owned(id)?
            .into_iter()
            .filter(|(_, presence)| *presence == Presence::Alive)
            .map(|(identity, _)| identity)
            .collect();
        Ok(capyctl_domain::completion::same_engine(&expected, &current).then_some(expected))
    }

    /// The journaled group of a launch is still `expected`, alive: every engine
    /// process of it and no other (a helper may be gone, ADR 0027).
    pub fn group_unchanged(
        self: &Arc<Self>,
        owned_handle: &str,
        expected: &[ProcessIdentity],
    ) -> Result<bool, JournalError> {
        let current: Vec<_> = self
            .inspect_owned(owned_handle)?
            .into_iter()
            .filter(|(_, presence)| *presence == Presence::Alive)
            .map(|(identity, _)| identity)
            .collect();
        Ok(capyctl_domain::completion::same_engine(expected, &current))
    }

    /// SPEC §§9.1, 13.1: persist how a begun Park or Restore ended, with its
    /// evidence, before the result is sent. A `Parked` or `Restored` claim
    /// holds only for the unchanged live group `begin_residency` returned;
    /// otherwise it is recorded `Uncertain` and the launch is quarantined.
    pub fn finish_residency(
        self: &Arc<Self>,
        command_id: &str,
        outcome: ResidencyOutcome,
        evidence: pb::ResidencyEvidence,
        expected: &[ProcessIdentity],
        observed_at_ms: i64,
    ) -> Result<ResidencyOutcome, JournalError> {
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        self.finish_locked(command_id, outcome, evidence, expected, observed_at_ms)
    }

    /// Caller holds the transition lock.
    fn finish_locked(
        self: &Arc<Self>,
        command_id: &str,
        outcome: ResidencyOutcome,
        mut evidence: pb::ResidencyEvidence,
        expected: &[ProcessIdentity],
        observed_at_ms: i64,
    ) -> Result<ResidencyOutcome, JournalError> {
        let command = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            load_command(&db, command_id)?
        };
        let owned = owned_handle(&command)?.to_string();
        let mut outcome = outcome;
        let mut result = self.execution_result(command_id, observed_at_ms)?;
        result.state = "completed".into();
        let claimed = matches!(
            outcome,
            ResidencyOutcome::Parked | ResidencyOutcome::Restored
        );
        let intent_held = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            residency_row(&db, &owned)?.is_some_and(|(_, by)| by == command_id)
        };
        // A claim needs the unchanged group and this command's own durable
        // intent (a Terminate that settled the launch meanwhile removed it).
        if claimed
            && (!intent_held
                || !capyctl_domain::completion::same_engine(expected, &alive_identities(&result)))
        {
            outcome = ResidencyOutcome::Uncertain;
        }
        let shape = |outcome: ResidencyOutcome,
                     result: &mut pb::MemberExecutionResult,
                     evidence: &mut pb::ResidencyEvidence| {
            evidence.state = match outcome {
                ResidencyOutcome::Unchanged => "unchanged",
                ResidencyOutcome::Parked => "parked",
                ResidencyOutcome::Restored => "restored",
                ResidencyOutcome::Uncertain => "unknown",
            }
            .into();
            result.residency = Some(evidence.clone());
            result.model_usable = outcome == ResidencyOutcome::Restored;
            result.observed_at_unix_ms = observed_at_ms;
        };
        shape(outcome, &mut result, &mut evidence);
        if capyctl_protocol::execution::validate_result(&command, &result).is_err() {
            // A claim the evidence cannot carry (for example a group that is no
            // longer live) is no claim: the outcome is unknown.
            if outcome == ResidencyOutcome::Unchanged {
                return Err(JournalError::Conflict);
            }
            outcome = ResidencyOutcome::Uncertain;
            shape(outcome, &mut result, &mut evidence);
            capyctl_protocol::execution::validate_result(&command, &result)
                .map_err(|_| JournalError::Conflict)?;
        }
        let mut db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let tx = db.transaction()?;
        let row = residency_row(&tx, &owned)?;
        let mine = row.as_ref().is_some_and(|(_, by)| by == command_id);
        let parking = matches!(command.action, MemberAction::Park { .. });
        match outcome {
            ResidencyOutcome::Unchanged if mine && parking => {
                tx.execute("DELETE FROM residency WHERE owner=?1", [&owned])?;
            }
            ResidencyOutcome::Unchanged if mine => {
                tx.execute(
                    "UPDATE residency SET state='parked' WHERE owner=?1",
                    [&owned],
                )?;
            }
            ResidencyOutcome::Unchanged => {}
            ResidencyOutcome::Parked if mine && parking => {
                tx.execute(
                    "UPDATE residency SET state='parked' WHERE owner=?1",
                    [&owned],
                )?;
            }
            ResidencyOutcome::Restored if mine && !parking => {
                tx.execute("DELETE FROM residency WHERE owner=?1", [&owned])?;
            }
            ResidencyOutcome::Uncertain if mine => {
                tx.execute(
                    "UPDATE residency SET state='uncertain' WHERE owner=?1",
                    [&owned],
                )?;
            }
            // The intent is gone (a Terminate settled the launch): the result
            // is recorded, and there is no residency left to quarantine.
            ResidencyOutcome::Uncertain => {}
            _ => return Err(JournalError::Conflict),
        }
        tx.execute(
            "UPDATE commands SET state=3 WHERE command_id=?1 AND claim=0",
            [command_id],
        )?;
        tx.execute("INSERT INTO native_results(command_id,result) VALUES(?1,?2) ON CONFLICT(command_id) DO UPDATE SET result=excluded.result", params![command_id, result.encode_to_vec()])?;
        tx.commit()?;
        Ok(outcome)
    }

    /// The durable residency of a launch: `None` while resident, otherwise one
    /// of `parking`, `parked`, `restoring` or `uncertain`.
    pub fn residency_of(&self, owned_handle: &str) -> Result<Option<String>, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        Ok(residency_row(&db, owned_handle)?.map(|(state, _)| state))
    }

    /// Current physical observations accompany retained command identity. A
    /// journal state alone never claims readiness or releases controller ownership.
    pub fn execution_result(
        self: &Arc<Self>,
        command_id: &str,
        observed_at_ms: i64,
    ) -> Result<pb::MemberExecutionResult, JournalError> {
        self.validate()?;
        let (command, record) = {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            (load_command(&db, command_id)?, record(&db, command_id)?)
        };
        let handle = match &command.action {
            MemberAction::Terminate { owned_handle, .. }
            | MemberAction::Probe { owned_handle }
            | MemberAction::Park { owned_handle }
            | MemberAction::Restore { owned_handle, .. } => owned_handle.clone(),
            MemberAction::Launch { .. } | MemberAction::LaunchSingle(_) => command_id.into(),
            _ => String::new(),
        };
        let unknown_probe_target = matches!(command.action, MemberAction::Probe { .. }) && {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM commands WHERE command_id=?1)",
                [&handle],
                |r| r.get::<_, bool>(0),
            )?
        };
        // ADR 0016: a Terminate of a handle this host has no launch record of
        // (it was fenced when first named, typically because the journal was
        // lost and the host re-enrolled) observes the identities the server
        // recorded. Nothing is signalled, adopted or journaled as owned; each
        // identity's presence is reported as observed now, so the server can
        // settle the launch on gone evidence by identity and never while any
        // of them is alive.
        let recorded_only = match &command.action {
            MemberAction::Terminate { recorded, .. } if !recorded.is_empty() => {
                let db = self.db.lock().map_err(|_| JournalError::Storage)?;
                fenced_handle(&db, &handle)?.then(|| recorded.clone())
            }
            _ => None,
        };
        let (processes, claim_retained) = if handle.is_empty() {
            (Vec::new(), record.claim_retained)
        } else if unknown_probe_target {
            // A probe of a launch this host never accepted observes nothing.
            (Vec::new(), false)
        } else if let Some(recorded) = recorded_only {
            let observed = recorded
                .into_iter()
                .map(|identity| {
                    let presence = capyctl_launchers::process_absence::presence(&identity);
                    (identity, presence)
                })
                .collect();
            (observed, false)
        } else {
            let observations = self.inspect_owned(&handle)?;
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            let retained = crate::journal::record(&db, &handle)?.claim_retained;
            (observations, retained)
        };
        // ADR 0028 §4, §8: the launch these processes belong to, for the
        // roles a group worker reports them under.
        let owner = if handle.is_empty() || handle == command_id {
            Some(command.clone())
        } else {
            let db = self.db.lock().map_err(|_| JournalError::Storage)?;
            load_command(&db, &handle).ok()
        };
        let bound = |action: &MemberAction| match action.launch_plan() {
            Some(plan) => (plan.binding_id.clone(), plan.incarnation.clone()),
            None => (String::new(), String::new()),
        };
        let (binding_id, incarnation) = match &command.action {
            // A probe, park or restore reports on the binding of the launch it
            // names.
            MemberAction::Probe { owned_handle }
            | MemberAction::Park { owned_handle }
            | MemberAction::Restore { owned_handle, .. } => {
                let db = self.db.lock().map_err(|_| JournalError::Storage)?;
                load_command(&db, owned_handle)
                    .map(|owner| bound(&owner.action))
                    .unwrap_or_default()
            }
            action => bound(action),
        };
        let mut result = pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: match record.state {
                CommandState::Accepted => "accepted",
                CommandState::Attempted => "attempted",
                CommandState::Launched => "launched",
                CommandState::Completed => "completed",
                CommandState::Tombstone => "tombstone",
            }
            .into(),
            owned_handle: handle.clone(),
            processes: processes
                .into_iter()
                .map(|(p, presence)| pb::OwnedProcessObservation {
                    role: reported_role(owner.as_ref(), p.role),
                    pid: p.pid,
                    boot_id: p.boot_id,
                    start_ticks: p.start_ticks,
                    presence: match presence {
                        Presence::Alive => "alive",
                        Presence::Gone => "gone",
                        Presence::Unknown => "unknown",
                    }
                    .into(),
                })
                .collect(),
            observed_at_unix_ms: observed_at_ms,
            claim_retained,
            model_usable: false,
            binding_id,
            incarnation,
            // SPEC §§9.1, 10: Park and Restore always report residency; with no
            // persisted outcome (an attempt still running, or one a host crash
            // interrupted) it is unknown and claims nothing.
            residency: matches!(
                command.action,
                MemberAction::Park { .. } | MemberAction::Restore { .. }
            )
            .then(unknown_residency),
            // ADR 0014 §7: DigestCheckpoint is never journaled.
            checkpoint: None,
            // SPEC §13: a policy refusal is never journaled either.
            refused: String::new(),
            // SPEC §§6.4, 13.2: the session adds a launch failure's summary.
            launch_failure: String::new(),
            // ADR 0008: MaterializeSource is never journaled either.
            source: None,
            kernel_builds: Vec::new(),
            // ADR 0028 §11: whether this Terminate needed SIGKILL.
            escalated: matches!(command.action, MemberAction::Terminate { .. }) && {
                let db = self.db.lock().map_err(|_| JournalError::Storage)?;
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM terminate_escalations WHERE command_id=?1)",
                    [command_id],
                    |r| r.get::<_, bool>(0),
                )?
            },
        };
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        // A launch that is not resident (parking, parked, restoring or
        // quarantined) is never reported usable by an earlier readiness proof.
        let resident = handle.is_empty() || residency_row(&db, &handle)?.is_none();
        let saved: Option<Vec<u8>> = db
            .query_row(
                "SELECT result FROM native_results WHERE command_id=?1",
                [command_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(bytes) = saved {
            let previous = pb::MemberExecutionResult::decode(bytes.as_slice())
                .map_err(|_| JournalError::Storage)?;
            capyctl_protocol::execution::validate_result(&command, &previous)
                .map_err(|_| JournalError::Storage)?;
            // ADR 0027: the engine's own processes; a helper that exited
            // since the result was persisted does not change the launch.
            let alive = |report: &pb::MemberExecutionResult| {
                report
                    .processes
                    .iter()
                    .filter(|p| {
                        p.presence == "alive"
                            && !capyctl_domain::completion::is_helper_role(&p.role)
                    })
                    .map(|p| (p.role.clone(), p.pid, p.boot_id.clone(), p.start_ticks))
                    .collect::<std::collections::BTreeSet<_>>()
            };
            let reportable = match &command.action {
                MemberAction::Probe { .. }
                | MemberAction::Park { .. }
                | MemberAction::Restore { .. } => result.state == "completed",
                _ => result.state == "launched",
            };
            let unchanged =
                reportable && result.claim_retained && alive(&result) == alive(&previous);
            if let MemberAction::Park { .. } | MemberAction::Restore { .. } = &command.action {
                // SPEC §§9.1, 13.2: a persisted park or restore claim is
                // repeated only while the launch is still in that state with
                // the same live group; otherwise the replay claims nothing.
                let mut evidence = previous.residency.clone().unwrap_or_else(unknown_residency);
                let current = residency_row(&db, &handle)?.map(|(state, _)| state);
                let holds = unchanged
                    && match evidence.state.as_str() {
                        "parked" => current.as_deref() == Some("parked"),
                        "restored" => current.is_none(),
                        _ => true,
                    };
                if !holds && matches!(evidence.state.as_str(), "parked" | "restored") {
                    evidence.state = "unknown".into();
                }
                result.model_usable =
                    holds && evidence.state == "restored" && previous.model_usable;
                result.residency = Some(evidence);
                if result.model_usable {
                    result.observed_at_unix_ms = previous.observed_at_unix_ms;
                }
            } else if unchanged && resident {
                result.model_usable = previous.model_usable;
                // A physical process observation cannot refresh model readiness.
                result.observed_at_unix_ms = previous.observed_at_unix_ms;
                // ADR 0014 amendment A12: they travel with the readiness proof.
                if result.model_usable {
                    result.kernel_builds = previous.kernel_builds;
                }
            }
        }
        Ok(result)
    }

    pub fn inspect_owned(
        self: &Arc<Self>,
        owned_handle: &str,
    ) -> Result<Vec<(ProcessIdentity, Presence)>, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let r = record(&db, owned_handle)?;
        let tools = DurableProcessLaunch::new(Arc::new(JournalAssociation {
            journal: Arc::downgrade(self),
            command_id: owned_handle.into(),
            effect: None,
        }));
        Ok(r.processes
            .into_iter()
            .map(|p| {
                let presence = tools.present(&p);
                (p, presence)
            })
            .collect())
    }

    /// SPEC §13.2 (W13): every launch this host claims that is Ready or parked
    /// now (its native readiness recorded, not changing residency), with the
    /// exact group that readiness named. The processes to watch for an exit;
    /// for a parked launch the report is evidence only.
    pub fn ready_launches(
        &self,
    ) -> Result<Vec<(MemberCommand, Vec<ProcessIdentity>)>, JournalError> {
        self.validate()?;
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let mut ready = Vec::new();
        for claimed in claimed_launches(&db, "")? {
            // SPEC §13.2 (W13): a parked engine is watched too, report only, so
            // a group that lost a member while parked is never woken as the
            // one it was.
            if !matches!(claimed.phase, ClaimPhase::Ready | ClaimPhase::Parked) {
                continue;
            }
            let id = &claimed.command.identity.command_id;
            let saved: Vec<u8> = db.query_row(
                "SELECT result FROM native_results WHERE command_id=?1",
                [id],
                |r| r.get(0),
            )?;
            let saved = pb::MemberExecutionResult::decode(saved.as_slice())
                .map_err(|_| JournalError::Storage)?;
            let group = alive_identities(&saved);
            if !group.is_empty() {
                ready.push((claimed.command, group));
            }
        }
        Ok(ready)
    }

    /// SPEC §13.2 (W13): journal that `process` of the retained launch
    /// `owned_handle` exited, with how it ended when that was observed. The
    /// first observation is kept; the stored one is returned as
    /// `(code, signal, observed_at_ms)`. Evidence only, never a release.
    pub fn record_member_exit(
        &self,
        owned_handle: &str,
        process: &ProcessIdentity,
        status: (Option<i32>, Option<i32>),
        observed_at_ms: i64,
    ) -> Result<(Option<i32>, Option<i32>, i64), JournalError> {
        // SPEC §13.2: evidence is written under the transition lock, so it
        // never interleaves with a Terminate settling the same launch.
        let _gate = self.transition.lock().map_err(|_| JournalError::Storage)?;
        self.validate()?;
        if observed_at_ms < 0 || (status.0.is_some() && status.1.is_some()) {
            return Err(JournalError::Conflict);
        }
        let db = self.db.lock().map_err(|_| JournalError::Storage)?;
        let owned = record(&db, owned_handle)?;
        if !owned.claim_retained || !owned.processes.contains(process) {
            return Err(JournalError::Conflict);
        }
        // The reaper may record how the process ended just after `/proc` stops
        // showing it; a status learned later fills an unobserved one, once.
        db.execute(
            "INSERT INTO member_exits(command_id,role,pid,boot,ticks,code,signal,observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(command_id,pid,boot,ticks) DO UPDATE SET code=excluded.code,signal=excluded.signal
             WHERE member_exits.code IS NULL AND member_exits.signal IS NULL",
            params![owned_handle, process.role, process.pid, process.boot_id, process.start_ticks as i64, status.0, status.1, observed_at_ms],
        )?;
        Ok(db.query_row(
            "SELECT code,signal,observed_at FROM member_exits WHERE command_id=?1 AND pid=?2 AND boot=?3 AND ticks=?4",
            params![owned_handle, process.pid, process.boot_id, process.start_ticks as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }
}
fn alive_identities(result: &pb::MemberExecutionResult) -> Vec<ProcessIdentity> {
    result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .map(|p| ProcessIdentity {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
        })
        .collect()
}

/// ADR 0028 §4, §8: the role one process of a launch is reported under. A
/// group worker reports its member id (`worker-<r>`) for the process it
/// spawned (recorded as the launch's `api` leader) and prefixes every other
/// role with it, so its whole tree is named for the member. A single launch
/// and a group head report the roles as recorded.
fn reported_role(owner: Option<&MemberCommand>, role: String) -> String {
    let Some(owner) = owner else {
        return role;
    };
    match &owner.action {
        MemberAction::Launch { member, .. } if member.service_port == 0 => {
            let id = &owner.identity.member.member_id;
            if role == "api" {
                id.clone()
            } else {
                format!("{id}/{role}")
            }
        }
        _ => role,
    }
}
/// ADR 0016: whether `handle` names only the fence a Terminate wrote for a
/// launch this host never accepted (no body, operation `fenced:<command>`),
/// as opposed to a launch it accepted, compacted or not.
fn fenced_handle(db: &Connection, handle: &str) -> Result<bool, JournalError> {
    Ok(db
        .query_row(
            "SELECT body IS NULL AND operation LIKE 'fenced:%' FROM commands WHERE command_id=?1",
            [handle],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// ADR 0027: a process first seen in a launch's group after its readiness was
/// recorded is a helper, whatever started it: it is recorded for cleanup, and
/// its exit, like its presence, is not the engine's. It is numbered after every
/// helper already recorded, so no two recorded processes share a role. Before
/// readiness the observation's own roles stand.
fn after_readiness(
    recorded: &[ProcessIdentity],
    observed: Vec<ProcessIdentity>,
    ready: bool,
) -> Vec<ProcessIdentity> {
    if !ready {
        return observed;
    }
    let known = |p: &ProcessIdentity| {
        recorded
            .iter()
            .any(|r| r.pid == p.pid && r.boot_id == p.boot_id && r.start_ticks == p.start_ticks)
    };
    let mut next = recorded
        .iter()
        .filter_map(|p| {
            p.role
                .strip_prefix(capyctl_domain::completion::HELPER_ROLE_PREFIX)
                .and_then(|n| n.parse::<u64>().ok())
        })
        .max()
        .map_or(0, |n| n + 1);
    observed
        .into_iter()
        .filter(|p| !known(p))
        .map(|mut p| {
            if p.role != "api" {
                p.role = format!("{}{next}", capyctl_domain::completion::HELPER_ROLE_PREFIX);
                next += 1;
            }
            p
        })
        .collect()
}

fn sorted(mut identities: Vec<ProcessIdentity>) -> Vec<ProcessIdentity> {
    identities.sort_by(|a, b| {
        (&a.role, a.pid, &a.boot_id, a.start_ticks).cmp(&(
            &b.role,
            b.pid,
            &b.boot_id,
            b.start_ticks,
        ))
    });
    identities
}

enum TerminateTarget {
    /// The journaled identities of the owned launch, possibly none.
    Owned(Vec<ProcessIdentity>),
    /// The handle names no command this host ever accepted.
    Unknown,
}

/// A retained launch may only be acted on by commands for the same member,
/// deployment and approved profile it was launched under.
fn same_owner(owner: &MemberCommand, command: &MemberCommand) -> bool {
    owner.identity.member == command.identity.member
        && owner.identity.deployment_id == command.identity.deployment_id
        && owner.identity.profile_fingerprint == command.identity.profile_fingerprint
}

/// ADR 0013 §5: the instance whose assignment fences `command`. A command
/// naming a launch this host journaled for the same deployment (Terminate,
/// Probe, Park, Restore) belongs to that launch's instance, whatever older
/// controller built it; an instance it does declare must agree. Anything else
/// is the instance its identity declares (0 before instances existed).
fn fence_instance(db: &Connection, command: &MemberCommand) -> Result<u32, JournalError> {
    let declared = command.identity.instance_index;
    let named = match &command.action {
        MemberAction::Terminate { owned_handle, .. }
        | MemberAction::Probe { owned_handle }
        | MemberAction::Park { owned_handle }
        | MemberAction::Restore { owned_handle, .. } => owned_handle,
        _ => return Ok(declared),
    };
    let owner: Option<(String, i64)> = db
        .query_row(
            "SELECT deployment,instance FROM commands WHERE command_id=?1",
            [named],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match owner {
        Some((deployment, instance)) if deployment == command.identity.deployment_id => {
            let instance = u32::try_from(instance).map_err(|_| JournalError::Storage)?;
            if declared != 0 && declared != instance {
                return Err(JournalError::Conflict);
            }
            Ok(instance)
        }
        _ => Ok(declared),
    }
}

fn unknown_residency() -> pb::ResidencyEvidence {
    pb::ResidencyEvidence {
        state: "unknown".into(),
        mem_available_before_bytes: -1,
        mem_available_after_bytes: -1,
        milestones: Vec::new(),
    }
}

fn owned_handle(command: &MemberCommand) -> Result<&str, JournalError> {
    match &command.action {
        MemberAction::Park { owned_handle } | MemberAction::Restore { owned_handle, .. } => {
            Ok(owned_handle)
        }
        _ => Err(JournalError::Unauthorized),
    }
}

fn residency_row(db: &Connection, owner: &str) -> Result<Option<(String, String)>, JournalError> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT state,command_id FROM residency WHERE owner=?1",
            [owner],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if row
        .as_ref()
        .is_some_and(|(state, _)| !RESIDENCY_STATES.contains(&state.as_str()))
    {
        return Err(JournalError::Storage);
    }
    Ok(row)
}

/// The retained single launch a Park or Restore names: same member,
/// deployment and approved profile, never a compacted or foreign command.
fn residency_owner(
    db: &Connection,
    command: &MemberCommand,
) -> Result<MemberCommand, JournalError> {
    let owned = owned_handle(command)?;
    let body: Option<Option<Vec<u8>>> = db
        .query_row(
            "SELECT body FROM commands WHERE command_id=?1",
            [owned],
            |r| r.get(0),
        )
        .optional()?;
    let Some(Some(body)) = body else {
        return Err(JournalError::Unauthorized);
    };
    let owner = decode(&body)?;
    if !same_owner(&owner, command) || !matches!(owner.action, MemberAction::LaunchSingle(_)) {
        return Err(JournalError::Unauthorized);
    }
    Ok(owner)
}

/// SPEC §§9.1, 10: Park only from resident (ready), Restore only from parked.
/// Parking, restoring or quarantined launches accept neither; a Terminate
/// settles them.
fn residency_precondition(db: &Connection, command: &MemberCommand) -> Result<(), JournalError> {
    let state = residency_row(db, owned_handle(command)?)?.map(|(state, _)| state);
    match (&command.action, state.as_deref()) {
        (MemberAction::Park { .. }, None) | (MemberAction::Restore { .. }, Some("parked")) => {
            Ok(())
        }
        _ => Err(JournalError::Unauthorized),
    }
}

/// Local-only capability. The native adapter's launch renderer receives this
/// value after durable acceptance; no transport can invoke raw argv directly.
struct JournalLaunchTools {
    journal: Arc<HostJournal>,
    command: MemberCommand,
    session: u64,
    clock: Arc<dyn ExecutionClock>,
    tools: DurableProcessLaunch,
    incarnation: String,
    policy: Arc<dyn LocalExecutionPolicy + Send + Sync>,
    spent: AtomicBool,
}
impl JournalLaunchTools {
    fn launch(
        &self,
        incarnation: &str,
        command: &RenderedCommand,
        descriptors: Option<&ProtectedLaunchDescriptors>,
    ) -> Result<ProcessIdentity, capyctl_adapters::traits::RuntimeError> {
        use capyctl_adapters::traits::RuntimeError;
        let failure = || RuntimeError::Uncertain("journal launch authority unavailable".into());
        let _gate = self.journal.transition.lock().map_err(|_| failure())?;
        if self.spent.swap(true, Ordering::AcqRel) || incarnation != self.incarnation {
            return Err(failure());
        }
        self.journal.validate().map_err(|_| failure())?;
        self.journal
            .check_session(self.session)
            .map_err(|_| failure())?;
        self.policy
            .authorize(&self.command)
            .map_err(|_| failure())?;
        before_deadline(self.clock.as_ref(), self.command.identity.deadline_ms)
            .map_err(|_| failure())?;
        {
            let db = self.journal.db.lock().map_err(|_| failure())?;
            let assignment: (i64, i64) = db.query_row(
                "SELECT a.generation,a.revision FROM assignments a JOIN commands c ON c.deployment=a.deployment AND c.instance=a.instance WHERE c.command_id=?1",
                [&self.command.identity.command_id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(|_| failure())?;
            if assignment
                != (
                    self.command.identity.generation,
                    self.command.identity.revision,
                )
            {
                return Err(failure());
            }
            // SPEC §13.2: a Terminate that settled this launch while nothing had
            // spawned released its claim under this same lock. Starting an engine
            // now would create a process nobody retains.
            let owned: (i64, bool) = db
                .query_row(
                    "SELECT state,claim FROM commands WHERE command_id=?1",
                    [&self.command.identity.command_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| failure())?;
            if owned != (1, true) {
                return Err(failure());
            }
            // SPEC §§3.1, 7.3: the last local check before the engine starts
            // is that it still fits beside every other retained claim.
            let claimed =
                claimed_launches(&db, &self.command.identity.command_id).map_err(|_| failure())?;
            self.policy
                .admit_beside(&self.command, &claimed)
                .map_err(|_| failure())?;
        }
        let identity = match descriptors {
            Some(descriptors) => {
                self.tools
                    .spawn_durable_protected(incarnation, command, descriptors)
            }
            None => self.tools.spawn_durable(incarnation, command),
        }?;
        let db = self.journal.db.lock().map_err(|_| failure())?;
        db.execute(
            "UPDATE commands SET state=2 WHERE command_id=?1 AND state=1",
            [&self.command.identity.command_id],
        )
        .map_err(|_| failure())?;
        Ok(identity)
    }
    fn owns(&self, identity: &ProcessIdentity) -> bool {
        self.journal
            .db
            .lock()
            .ok()
            .and_then(|db| record(&db, &self.command.identity.command_id).ok())
            .is_some_and(|record| record.processes.contains(identity))
    }
}
impl OwnedProcessLaunch for JournalLaunchTools {
    fn spawn_durable(
        &self,
        incarnation: &str,
        command: &RenderedCommand,
    ) -> Result<ProcessIdentity, capyctl_adapters::traits::RuntimeError> {
        self.launch(incarnation, command, None)
    }
    fn spawn_durable_protected(
        &self,
        incarnation: &str,
        command: &RenderedCommand,
        descriptors: &ProtectedLaunchDescriptors,
    ) -> Result<ProcessIdentity, capyctl_adapters::traits::RuntimeError> {
        self.launch(incarnation, command, Some(descriptors))
    }
    fn present(&self, identity: &ProcessIdentity) -> Presence {
        if self.owns(identity) {
            self.tools.present(identity)
        } else {
            Presence::Unknown
        }
    }
    fn building(&self, api: &ProcessIdentity) -> bool {
        self.owns(api) && self.tools.building(api)
    }
    fn observe_group(
        &self,
        api: &ProcessIdentity,
    ) -> Result<Vec<ProcessIdentity>, capyctl_adapters::traits::RuntimeError> {
        use capyctl_adapters::traits::RuntimeError;
        if !self.owns(api) {
            return Err(RuntimeError::Uncertain("unowned process".into()));
        }
        let observed = self.tools.observe_group(api)?;
        let persist = || -> Result<(), JournalError> {
            self.journal.validate()?;
            let mut db = self.journal.db.lock().map_err(|_| JournalError::Storage)?;
            let tx = db.transaction()?;
            for identity in &observed {
                persist_process(&tx, &self.command.identity.command_id, identity)?;
            }
            tx.commit()?;
            Ok(())
        };
        persist().map_err(|_| {
            RuntimeError::Uncertain("process observation could not be retained".into())
        })?;
        Ok(observed)
    }
    fn terminate_owned(
        &self,
        _: &[ProcessIdentity],
        _: Duration,
    ) -> Result<(), capyctl_adapters::traits::RuntimeError> {
        // Cleanup requires its own accepted, fenced command.
        Err(capyctl_adapters::traits::RuntimeError::Unsupported)
    }
}

struct JournalAssociation {
    journal: Weak<HostJournal>,
    command_id: String,
    effect: Option<(Arc<dyn ExecutionClock>, i64, u64)>,
}
impl LaunchAssociation for JournalAssociation {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        let persist = || -> Result<(), JournalError> {
            let journal = self.journal.upgrade().ok_or(JournalError::Storage)?;
            journal.validate()?;
            let (clock, deadline, session) =
                self.effect.as_ref().ok_or(JournalError::Unauthorized)?;
            before_deadline(clock.as_ref(), *deadline)?;
            journal.check_session(*session)?;
            let mut db = journal.db.lock().map_err(|_| JournalError::Storage)?;
            let tx = db.transaction()?;
            let state: (i64, bool) = tx.query_row(
                "SELECT state,claim FROM commands WHERE command_id=?1",
                [&self.command_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if state != (1, true) {
                return Err(JournalError::Conflict);
            }
            persist_process(&tx, &self.command_id, identity)?;
            tx.commit()?;
            // If persistence crossed the deadline, refuse release of the gate;
            // the launcher disposes only its just-created child. Claim remains.
            before_deadline(clock.as_ref(), *deadline)?;
            journal.check_session(*session)?;
            Ok(())
        };
        persist()
            .map_err(|_| AssociationError::Uncertain("host ownership persistence failed".into()))
    }
}
fn persist_process(
    db: &Connection,
    command: &str,
    p: &ProcessIdentity,
) -> Result<(), JournalError> {
    if p.pid == 0
        || p.start_ticks == 0
        || p.start_ticks > i64::MAX as u64
        || p.boot_id.is_empty()
        || p.boot_id.len() > 64
        || p.role.is_empty()
        || p.role.len() > 64
    {
        return Err(JournalError::Conflict);
    }
    let existing: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM processes WHERE command_id=?1 AND pid=?2 AND boot=?3 AND ticks=?4)", params![command,p.pid,p.boot_id,p.start_ticks as i64], |r|r.get(0))?;
    let count: usize = db.query_row(
        "SELECT count(*) FROM processes WHERE command_id=?1",
        [command],
        |r| r.get(0),
    )?;
    if !existing && count >= MAX_PROCESSES {
        return Err(JournalError::Uncertain);
    }
    db.execute(
        "INSERT OR IGNORE INTO processes VALUES(?1,?2,?3,?4,?5)",
        params![command, p.role, p.pid, p.boot_id, p.start_ticks as i64],
    )?;
    Ok(())
}
fn record(db: &Connection, id: &str) -> Result<CommandRecord, JournalError> {
    let (sequence, state, claim): (i64, i64, bool) = db.query_row(
        "SELECT sequence,state,claim FROM commands WHERE command_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let processes = db
        .prepare(
            "SELECT role,pid,boot,ticks FROM processes WHERE command_id=?1 ORDER BY ticks,pid",
        )?
        .query_map([id], |r| {
            Ok(ProcessIdentity {
                role: r.get(0)?,
                pid: r.get(1)?,
                boot_id: r.get(2)?,
                start_ticks: r.get::<_, i64>(3)? as u64,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if processes.len() > MAX_PROCESSES
        || processes.iter().any(|p| {
            p.pid == 0
                || p.start_ticks == 0
                || p.start_ticks > i64::MAX as u64
                || p.role.is_empty()
                || p.role.len() > 64
                || p.boot_id.is_empty()
                || p.boot_id.len() > 64
        })
    {
        return Err(JournalError::Storage);
    }
    Ok(CommandRecord {
        sequence,
        command_id: id.into(),
        state: CommandState::decode(state)?,
        processes,
        claim_retained: claim,
    })
}
/// SPEC §§3.1, 7.3: every launch claim this host retains except `except`
/// (the command being admitted), with the phase durable state proves. A
/// claimed row always keeps its body: compaction never touches a claim.
fn claimed_launches(db: &Connection, except: &str) -> Result<Vec<ClaimedLaunch>, JournalError> {
    let rows: Vec<(String, Option<Vec<u8>>, i64)> = db
        .prepare("SELECT command_id,body,state FROM commands WHERE claim=1 AND command_id!=?1 ORDER BY sequence")?
        .query_map([except], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut claimed = Vec::with_capacity(rows.len());
    for (id, body, state) in rows {
        let command = decode(&body.ok_or(JournalError::Storage)?)?;
        let phase = match residency_row(db, &id)?.map(|(state, _)| state).as_deref() {
            Some("parked") => ClaimPhase::Parked,
            Some(_) => ClaimPhase::Changing,
            None => {
                let saved: Option<Vec<u8>> = db
                    .query_row(
                        "SELECT result FROM native_results WHERE command_id=?1",
                        [&id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let ready = CommandState::decode(state)? == CommandState::Launched
                    && saved
                        .map(|bytes| pb::MemberExecutionResult::decode(bytes.as_slice()))
                        .transpose()
                        .map_err(|_| JournalError::Storage)?
                        .is_some_and(|result| result.model_usable);
                if ready {
                    ClaimPhase::Ready
                } else {
                    ClaimPhase::Starting
                }
            }
        };
        claimed.push(ClaimedLaunch { command, phase });
    }
    Ok(claimed)
}
fn load_command(db: &Connection, id: &str) -> Result<MemberCommand, JournalError> {
    let body: Vec<u8> =
        db.query_row("SELECT body FROM commands WHERE command_id=?1", [id], |r| {
            r.get(0)
        })?;
    decode(&body)
}
fn decode(body: &[u8]) -> Result<MemberCommand, JournalError> {
    if body.len() > MAX_COMMAND_BYTES {
        return Err(JournalError::Storage);
    }
    let wire = pb::ExecuteMember::decode(body).map_err(|_| JournalError::Storage)?;
    let command = MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
    })
    .map_err(|_| JournalError::Storage)?;
    command.verify_digest().map_err(|_| JournalError::Storage)?;
    Ok(command)
}
fn validate_files(path: &Path) -> Result<(), JournalError> {
    let uid = unsafe { libc::geteuid() };
    for name in [
        "commands.sqlite",
        "commands.sqlite-journal",
        "commands.sqlite-wal",
        "commands.sqlite-shm",
    ] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(m)
                if m.is_file()
                    && m.uid() == uid
                    && m.mode() & 0o7777 == 0o600
                    && m.nlink() == 1 =>
            {
                if name.ends_with("-wal") || name.ends_with("-shm") {
                    return Err(JournalError::Storage);
                }
            }
            Ok(_) => return Err(JournalError::Storage),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(JournalError::Storage),
        }
    }
    Ok(())
}

#[cfg(test)]
mod helper_tests {
    use super::after_readiness;
    use capyctl_domain::completion::ProcessIdentity;

    fn p(role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid),
        }
    }

    /// ADR 0027: after readiness, a process first seen on an agent restart is
    /// recorded as a helper numbered past the recorded ones, so no two recorded
    /// processes share a role; one already recorded is not recorded again.
    /// Before readiness the observation's roles stand.
    #[test]
    fn a_process_first_seen_after_readiness_is_a_new_helper() {
        let recorded = vec![
            p("api", 1),
            p("worker-0", 2),
            p("helper-0", 3),
            p("helper-4", 4),
        ];
        let observed = vec![
            p("api", 1),
            p("worker-0", 2),
            p("worker-1", 9),
            p("helper-0", 10),
        ];
        assert_eq!(
            after_readiness(&recorded, observed.clone(), true),
            vec![p("helper-5", 9), p("helper-6", 10)]
        );
        assert_eq!(
            after_readiness(&recorded, observed.clone(), false),
            observed
        );
    }
}
