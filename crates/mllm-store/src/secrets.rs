//! The engine key at rest (spec §3). The database alone cannot recover a key: the
//! identity key lives in `<state_dir>/identity/secrets.key`, owner-only, and never
//! enters the database. A per-launch key is sealed with XChaCha20-Poly1305 under
//! that identity key, with binding id, incarnation and role as associated data so
//! a row copied between bindings or roles does not authenticate.
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::StoreError;

/// The 32-byte identity key that seals every engine key. Loaded once at startup
/// from an owner-only file outside the database (spec §3).
#[derive(Clone)]
pub struct SecretsKey([u8; 32]);

impl SecretsKey {
    /// Loads the identity key from `path`, creating a fresh random one, owner-only
    /// (mode 0600), the first time the file does not exist.
    ///
    /// Spec §3: the identity key is owner-only, and that holds for a file this
    /// process did not create as much as for one it did. A key readable by anyone
    /// else is refused rather than used, the same way `Store::open` refuses a
    /// loosely permissioned database. A file of the wrong length is corrupt data,
    /// not a lifecycle conflict, so it is reported as such: every other caller
    /// reads `Conflict` as "the request clashes with store state".
    pub fn load_or_create(path: &Path) -> Result<Self, StoreError> {
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "identity key {} is mode {:o}; it must be readable by its owner only",
                        path.display(),
                        mode & 0o777
                    ),
                )));
            }
        }
        // SPEC §13.3: only a file that is absent is created. Any other read
        // failure on an existing file is reported, never papered over by minting
        // a new key: a new key would leave every engine secret sealed under the
        // old one unrecoverable, and a restart could then re-attach to nothing.
        match std::fs::read(path) {
            Ok(bytes) => {
                let key: [u8; 32] = bytes.try_into().map_err(|_| {
                    StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("identity key {} is not 32 bytes", path.display()),
                    ))
                })?;
                return Ok(Self(key));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::Io(error)),
        }
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        std::io::Write::write_all(&mut file, &key)?;
        Ok(Self(key))
    }

    /// An identity key held only in memory, for tests and other short-lived stores
    /// that never persist an identity key file.
    pub fn generate_ephemeral() -> Self {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        Self(key)
    }

    /// A stable, non-secret fingerprint suitable for logging and equality checks.
    pub fn fingerprint(&self) -> String {
        hex::encode(Sha256::digest(self.0))
    }
}

impl std::fmt::Debug for SecretsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretsKey(..)")
    }
}

/// A fresh random 32-byte engine key for one launch.
pub fn new_engine_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    key
}

/// Which launch credential a sealed engine key is. A binding can carry one key
/// per role: vLLM seals only `Inference`; SGLang seals `Inference` and `Admin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretRole {
    Inference,
    Admin,
}

impl SecretRole {
    /// The stored string form of the role, also joined into the sealing AAD.
    pub fn as_str(self) -> &'static str {
        match self {
            SecretRole::Inference => "inference",
            SecretRole::Admin => "admin",
        }
    }
}

impl std::str::FromStr for SecretRole {
    type Err = String;

    /// Parses the stored string form. Any other string is an error: the
    /// table's check constraint already keeps foreign strings out of the
    /// `role` column, so this only ever rejects what callers invent.
    fn from_str(role: &str) -> Result<Self, Self::Err> {
        match role {
            "inference" => Ok(SecretRole::Inference),
            "admin" => Ok(SecretRole::Admin),
            other => Err(format!("unknown secret role {other:?}")),
        }
    }
}

impl crate::Store {
    /// Installs the identity key this store seals and opens engine keys with.
    /// Without it, `store_engine_key` and `engine_key` return `StoreError::Conflict`.
    pub fn set_secrets_key(&mut self, key: SecretsKey) {
        self.secrets = Some(key);
    }

    fn cipher(&self) -> Result<XChaCha20Poly1305, StoreError> {
        let key = self.secrets.as_ref().ok_or(StoreError::Conflict)?;
        Ok(XChaCha20Poly1305::new((&key.0).into()))
    }

    /// Seals `key` for `binding_id`/`incarnation` under `role` and stores it,
    /// replacing any existing row for the binding and role. Spec §3: the
    /// binding id, incarnation and role are associated data, not part of the
    /// ciphertext, so a row moved to another binding, incarnation or role
    /// fails to authenticate on read.
    pub fn store_engine_key(
        &self,
        binding_id: &str,
        incarnation: &str,
        key: &[u8; 32],
        role: SecretRole,
    ) -> Result<(), StoreError> {
        let mut nonce = [0u8; 24];
        OsRng.fill_bytes(&mut nonce);
        let aad = format!("{binding_id}\0{incarnation}\0{}", role.as_str());
        let ciphertext = self
            .cipher()?
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: key,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| StoreError::Conflict)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO engine_secrets(binding_id,role,incarnation,nonce,ciphertext) VALUES(?1,?2,?3,?4,?5)",
            params![binding_id, role.as_str(), incarnation, nonce.to_vec(), ciphertext],
        )?;
        Ok(())
    }

    /// Opens the engine key stored for `binding_id`/`incarnation` under `role`,
    /// or `None` if no row exists. Fails with `StoreError::Conflict` when the
    /// identity key is not installed, or when decryption does not authenticate
    /// against the requested binding id, incarnation and role.
    pub fn engine_key(
        &self,
        binding_id: &str,
        incarnation: &str,
        role: SecretRole,
    ) -> Result<Option<[u8; 32]>, StoreError> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT nonce,ciphertext FROM engine_secrets WHERE binding_id=?1 AND incarnation=?2 AND role=?3",
                params![binding_id, incarnation, role.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((nonce, ciphertext)) = row else {
            return Ok(None);
        };
        let aad = format!("{binding_id}\0{incarnation}\0{}", role.as_str());
        let plain = self
            .cipher()?
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| StoreError::Conflict)?;
        plain.try_into().map(Some).map_err(|_| StoreError::Conflict)
    }

    /// Deletes every engine key row (all roles) for `binding_id`, if any.
    /// Called on ordinary cleanup so no encrypted key outlives the binding it
    /// was issued for.
    pub fn delete_engine_keys(&self, binding_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM engine_secrets WHERE binding_id=?1",
            [binding_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn seed_binding(store: &crate::Store, binding_id: &str, incarnation: &str) {
    store
        .conn
        .execute(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES(?1,?1,'model','ready',1,0,1,1)",
            [binding_id],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES(?1,?1,1,?2,'managed','{}','[]','reserved')",
            params![binding_id, incarnation],
        )
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key round-trips only under the same identity key and the same row identity.
    /// SPEC §13.3: a sealed engine key is bound to the row it was sealed for, so
    /// a row moved to another binding does not open.
    // T37
    #[test]
    fn engine_key_round_trips_and_is_bound_to_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretsKey::load_or_create(&dir.path().join("secrets.key")).unwrap();
        let mut store = crate::Store::open_in_memory().unwrap();
        store.set_secrets_key(secrets);
        seed_binding(&store, "b1", "inc1");
        // A second real binding, so the row-copy below still satisfies the
        // engine_secrets -> runtime_bindings foreign key.
        seed_binding(&store, "b2", "inc2");
        let key = [7u8; 32];
        store
            .store_engine_key("b1", "inc1", &key, SecretRole::Inference)
            .unwrap();
        assert_eq!(
            store
                .engine_key("b1", "inc1", SecretRole::Inference)
                .unwrap(),
            Some(key)
        );
        // Same ciphertext under another binding does not authenticate.
        store
            .conn
            .execute(
                "UPDATE engine_secrets SET binding_id='b2' WHERE binding_id='b1'",
                [],
            )
            .unwrap();
        assert!(store
            .engine_key("b2", "inc1", SecretRole::Inference)
            .is_err());
    }

    /// SGLang seals two keys per launch, one per role. Each round-trips
    /// independently under the same binding and incarnation.
    // T37
    #[test]
    fn both_roles_round_trip_independently() {
        let mut store = crate::Store::open_in_memory().unwrap();
        store.set_secrets_key(SecretsKey::generate_ephemeral());
        seed_binding(&store, "b1", "inc1");
        let inference = [1u8; 32];
        let admin = [2u8; 32];
        store
            .store_engine_key("b1", "inc1", &inference, SecretRole::Inference)
            .unwrap();
        store
            .store_engine_key("b1", "inc1", &admin, SecretRole::Admin)
            .unwrap();
        assert_eq!(
            store
                .engine_key("b1", "inc1", SecretRole::Inference)
                .unwrap(),
            Some(inference)
        );
        assert_eq!(
            store.engine_key("b1", "inc1", SecretRole::Admin).unwrap(),
            Some(admin)
        );
        // Storing one role again does not disturb the other.
        store
            .store_engine_key("b1", "inc1", &[3u8; 32], SecretRole::Inference)
            .unwrap();
        assert_eq!(
            store
                .engine_key("b1", "inc1", SecretRole::Inference)
                .unwrap(),
            Some([3u8; 32])
        );
        assert_eq!(
            store.engine_key("b1", "inc1", SecretRole::Admin).unwrap(),
            Some(admin)
        );
    }

    /// The role is associated data: a ciphertext copied from one role's row to
    /// another does not authenticate when opened as the copied-to role.
    // T37
    #[test]
    fn a_row_copied_between_roles_does_not_authenticate() {
        let mut store = crate::Store::open_in_memory().unwrap();
        store.set_secrets_key(SecretsKey::generate_ephemeral());
        seed_binding(&store, "b1", "inc1");
        let admin = [2u8; 32];
        store
            .store_engine_key("b1", "inc1", &admin, SecretRole::Admin)
            .unwrap();
        // Copy the admin ciphertext into the inference role's row.
        store
            .conn
            .execute(
                "INSERT INTO engine_secrets(binding_id,role,incarnation,nonce,ciphertext)
                 SELECT binding_id,'inference',incarnation,nonce,ciphertext
                 FROM engine_secrets WHERE role='admin'",
                [],
            )
            .unwrap();
        assert!(
            store
                .engine_key("b1", "inc1", SecretRole::Inference)
                .is_err(),
            "a row copied across roles must fail to authenticate"
        );
    }

    /// Cleanup releases all roles for a binding: no sealed key of any role
    /// outlives the binding it was issued for.
    // T37
    #[test]
    fn delete_engine_keys_removes_every_role() {
        let mut store = crate::Store::open_in_memory().unwrap();
        store.set_secrets_key(SecretsKey::generate_ephemeral());
        seed_binding(&store, "b1", "inc1");
        store
            .store_engine_key("b1", "inc1", &[1u8; 32], SecretRole::Inference)
            .unwrap();
        store
            .store_engine_key("b1", "inc1", &[2u8; 32], SecretRole::Admin)
            .unwrap();
        store.delete_engine_keys("b1").unwrap();
        assert_eq!(
            store
                .engine_key("b1", "inc1", SecretRole::Inference)
                .unwrap(),
            None
        );
        assert_eq!(
            store.engine_key("b1", "inc1", SecretRole::Admin).unwrap(),
            None
        );
    }

    /// SPEC §13.3 credential handling, and T02: the local identity key is created
    /// once and read back, never regenerated over a live one.
    // T37
    // T02
    #[test]
    fn the_identity_key_file_is_owner_only_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.key");
        let a = SecretsKey::load_or_create(&path).unwrap();
        let b = SecretsKey::load_or_create(&path).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// Spec §3: an identity key that somebody else can read is refused, and a file
    /// that is not a key is corrupt data rather than a lifecycle conflict.
    // T37
    #[test]
    fn a_readable_or_malformed_identity_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let loose = dir.path().join("loose.key");
        std::fs::write(&loose, [0u8; 32]).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = SecretsKey::load_or_create(&loose).unwrap_err();
        let StoreError::Io(io) = error else {
            panic!("a group-readable key is an ownership failure, not a conflict");
        };
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);

        let short = dir.path().join("short.key");
        std::fs::write(&short, [0u8; 16]).unwrap();
        std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = SecretsKey::load_or_create(&short).unwrap_err();
        let StoreError::Io(io) = error else {
            panic!("a wrong-length key is invalid data, not a conflict");
        };
        assert_eq!(io.kind(), std::io::ErrorKind::InvalidData);
    }
}
