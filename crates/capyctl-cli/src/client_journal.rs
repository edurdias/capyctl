//! SPEC §13.1, T13: persist the exact mutation before transmission so a new CLI
//! process can recover the same accepted operation without recomputing fences.
use crate::output::StructuredError;
use capyctl_agent::identity_storage::IdentityDirectory;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
};
const LIMIT: u64 = 8 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Mutation {
    pub key: String,
    pub path: String,
    pub body: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    endpoint: String,
    authorization: String,
    intent: Value,
    mutations: BTreeMap<String, Mutation>,
}
pub(crate) struct RequestJournal {
    id: String,
    path: PathBuf,
    record: Mutex<Record>,
    guard: IdentityDirectory,
}
fn invalid() -> StructuredError {
    StructuredError { code:"invalid_config", message:"Request identity conflicts with its saved command, or its private journal is unavailable".into() }
}
fn directory(path: &Path) -> Result<(), StructuredError> {
    match fs::symlink_metadata(path) {
        Ok(m)
            if m.is_dir()
                && m.mode() & 0o7777 == 0o700
                && m.uid() == unsafe { libc::geteuid() } =>
        {
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => directory(path),
                Err(_) => Err(invalid()),
            }
        }
        _ => Err(invalid()),
    }
}
impl RequestJournal {
    pub fn open(
        root: &Path,
        id: &str,
        endpoint: &str,
        authorization: String,
        intent: Value,
    ) -> Result<Self, StructuredError> {
        // SPEC §13: only the canonical spelling names a journal, so one
        // request identity can never own two journals or two keys.
        if id.len() != 26 || id.parse::<ulid::Ulid>().map(|u| u.to_string()).as_deref() != Ok(id) {
            return Err(invalid());
        }
        let parent = root.join("requests");
        directory(&parent)?;
        let dir = parent.join(id);
        directory(&dir)?;
        let guard = IdentityDirectory::open(&dir).map_err(|_| invalid())?;
        let initialized = guard
            .read_bundle("initialized")
            .map_err(|_| invalid())?
            .is_some();
        let path = dir.join("request.json");
        let record = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => {
                let meta = file.metadata().map_err(|_| invalid())?;
                if !meta.is_file()
                    || meta.mode() & 0o7777 != 0o600
                    || meta.uid() != unsafe { libc::geteuid() }
                    || meta.nlink() != 1
                    || meta.len() > LIMIT
                {
                    return Err(invalid());
                }
                let mut bytes = Vec::new();
                file.take(LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| invalid())?;
                if bytes.len() as u64 > LIMIT {
                    return Err(invalid());
                }
                let old: Record = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
                if old.version != 1
                    || old.endpoint != endpoint
                    || old.authorization != authorization
                    || old.intent != intent
                {
                    return Err(invalid());
                }
                old
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !initialized => Record {
                version: 1,
                endpoint: endpoint.into(),
                authorization,
                intent,
                mutations: BTreeMap::new(),
            },
            Err(_) => return Err(invalid()),
        };
        let journal = Self {
            id: id.into(),
            path,
            record: Mutex::new(record),
            guard,
        };
        {
            let record = journal.record.lock().map_err(|_| invalid())?;
            journal.persist(&record)?;
        }
        if !initialized {
            journal
                .guard
                .create_bundle("initialized", b"1")
                .map_err(|_| invalid())?;
        }
        Ok(journal)
    }
    fn persist(&self, record: &Record) -> Result<(), StructuredError> {
        self.guard.validate().map_err(|_| invalid())?;
        let bytes = serde_json::to_vec(record).map_err(|_| invalid())?;
        if bytes.len() as u64 > LIMIT {
            return Err(invalid());
        }
        let parent = self.path.parent().ok_or_else(invalid)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|_| invalid())?;
        temp.write_all(&bytes).map_err(|_| invalid())?;
        temp.as_file().sync_all().map_err(|_| invalid())?;
        self.guard.validate().map_err(|_| invalid())?;
        temp.persist(&self.path).map_err(|_| invalid())?;
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| invalid())?;
        Ok(())
    }
    pub fn saved(&self, step: &str) -> Result<Option<Mutation>, StructuredError> {
        self.guard.validate().map_err(|_| invalid())?;
        Ok(self
            .record
            .lock()
            .map_err(|_| invalid())?
            .mutations
            .get(step)
            .cloned())
    }
    /// The request identity, which recovers this command (`--request-id`).
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Drop a step whose saved mutation the server definitively refused, so a
    /// later attempt commits a fresh body under the same key. Only for a
    /// refusal that proves nothing was accepted under the key: an accepted
    /// command always replays its receipt, so its step is never dropped.
    pub fn forget(&self, step: &str) -> Result<(), StructuredError> {
        let mut record = self.record.lock().map_err(|_| invalid())?;
        if let Some(old) = record.mutations.remove(step) {
            if let Err(error) = self.persist(&record) {
                record.mutations.insert(step.into(), old);
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn prepare(
        &self,
        step: &str,
        path: &str,
        body: Value,
    ) -> Result<Mutation, StructuredError> {
        let mut record = self.record.lock().map_err(|_| invalid())?;
        if let Some(old) = record.mutations.get(step) {
            if old.path != path || old.body != body {
                return Err(invalid());
            }
            return Ok(old.clone());
        }
        let mutation = Mutation {
            key: format!("{}-{step}", self.id),
            path: path.into(),
            body,
        };
        record.mutations.insert(step.into(), mutation.clone());
        if let Err(error) = self.persist(&record) {
            record.mutations.remove(step);
            return Err(error);
        }
        Ok(mutation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    // T13: reopen recovers exact deadline/revision; changed intent and partial state fail closed.
    #[test]
    fn replay_preserves_command_and_refuses_identity_reuse_or_missing_journal() {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let id = ulid::Ulid::new().to_string();
        let intent = serde_json::json!({"command":"start","deployment":"dep"});
        let open = || {
            RequestJournal::open(
                root.path(),
                &id,
                "endpoint",
                "credential-digest".into(),
                intent.clone(),
            )
        };
        let journal = open().unwrap();
        assert!(open().is_err(), "concurrent mutation writer accepted");
        let body = serde_json::json!({"action":"start","expected_revision":1,"deadline_ms":123});
        let original = journal
            .prepare("action", "/deployments/dep/actions", body.clone())
            .unwrap();
        drop(journal);
        let recovered = open().unwrap();
        let replay = recovered.saved("action").unwrap().unwrap();
        assert_eq!(replay.body, body);
        assert_eq!(replay.key, original.key);
        assert!(recovered
            .prepare(
                "action",
                "/deployments/dep/actions",
                serde_json::json!({"action":"stop"})
            )
            .is_err());
        drop(recovered);
        assert!(RequestJournal::open(
            root.path(),
            &id,
            "other-server",
            "credential-digest".into(),
            intent.clone()
        )
        .is_err());
        assert!(
            RequestJournal::open(
                root.path(),
                &id.to_ascii_lowercase(),
                "endpoint",
                "credential-digest".into(),
                intent.clone()
            )
            .is_err(),
            "a non-canonical spelling opened a second journal"
        );
        fs::remove_file(root.path().join("requests").join(&id).join("request.json")).unwrap();
        assert!(open().is_err(), "missing established journal regenerated");
    }
}
