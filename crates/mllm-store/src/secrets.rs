//! The engine key at rest (spec §3). The database alone cannot recover a key: the
//! identity key lives in `<state_dir>/identity/secrets.key`, owner-only, and never
//! enters the database. A per-launch key is sealed with XChaCha20-Poly1305 under
//! that identity key, with binding id and incarnation as associated data so a row
//! copied between bindings does not authenticate.
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
    pub fn load_or_create(path: &Path) -> Result<Self, StoreError> {
        if let Ok(bytes) = std::fs::read(path) {
            let key: [u8; 32] = bytes
                .try_into()
                .map_err(|_| StoreError::Conflict)?;
            return Ok(Self(key));
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

    /// Seals `key` for `binding_id`/`incarnation` and stores it, replacing any
    /// existing row for the binding. Spec §3: the binding id and incarnation are
    /// associated data, not part of the ciphertext, so a row moved to another
    /// binding or incarnation fails to authenticate on read.
    pub fn store_engine_key(
        &self,
        binding_id: &str,
        incarnation: &str,
        key: &[u8; 32],
    ) -> Result<(), StoreError> {
        let mut nonce = [0u8; 24];
        OsRng.fill_bytes(&mut nonce);
        let aad = format!("{binding_id}\0{incarnation}");
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
            "INSERT OR REPLACE INTO engine_secrets(binding_id,incarnation,nonce,ciphertext) VALUES(?1,?2,?3,?4)",
            params![binding_id, incarnation, nonce.to_vec(), ciphertext],
        )?;
        Ok(())
    }

    /// Opens the engine key stored for `binding_id`/`incarnation`, or `None` if no
    /// row exists. Fails with `StoreError::Conflict` when the identity key is not
    /// installed, or when decryption does not authenticate against the requested
    /// binding id and incarnation.
    pub fn engine_key(
        &self,
        binding_id: &str,
        incarnation: &str,
    ) -> Result<Option<[u8; 32]>, StoreError> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT nonce,ciphertext FROM engine_secrets WHERE binding_id=?1 AND incarnation=?2",
                params![binding_id, incarnation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((nonce, ciphertext)) = row else {
            return Ok(None);
        };
        let aad = format!("{binding_id}\0{incarnation}");
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

    /// Deletes the engine key row for `binding_id`, if any. Called on ordinary
    /// cleanup so no encrypted key outlives the binding it was issued for.
    pub fn delete_engine_key(&self, binding_id: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM engine_secrets WHERE binding_id=?1", [binding_id])?;
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
            "INSERT INTO runtime_bindings VALUES(?1,?1,1,?2,'managed','{}','[]','reserved')",
            params![binding_id, incarnation],
        )
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key round-trips only under the same identity key and the same row identity.
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
        store.store_engine_key("b1", "inc1", &key).unwrap();
        assert_eq!(store.engine_key("b1", "inc1").unwrap(), Some(key));
        // Same ciphertext under another binding does not authenticate.
        store
            .conn
            .execute(
                "UPDATE engine_secrets SET binding_id='b2' WHERE binding_id='b1'",
                [],
            )
            .unwrap();
        assert!(store.engine_key("b2", "inc1").is_err());
    }

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
}
