//! SPEC §4.1: persist authorization and the exact issuance result in one transaction.
//! Only digests enter SQLite; invitation bearer material never does.
use crate::{Store, StoreError};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

#[derive(Clone, PartialEq, Eq)]
pub struct CertificateRecord {
    pub host_id: String,
    pub fingerprint: String,
    pub certificate_pem: String,
    pub expires_unix: i64,
}
pub struct Redemption {
    pub invitation_digest: String,
    pub transaction_id: String,
    pub host_name: String,
    pub key_digest: String,
    pub csr_digest: String,
}
/// SPEC §4.1: the outcome of revoking one enrolled host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revocation {
    pub host_id: String,
    pub host_name: String,
    /// False when the host was already revoked; the retry changed nothing.
    pub newly_revoked: bool,
}
#[derive(serde::Serialize)]
pub struct EnrolledHost {
    pub host_id: String,
    pub host_name: String,
    pub revoked: bool,
}
impl std::fmt::Debug for CertificateRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateRecord")
            .field("host_id", &self.host_id)
            .finish_non_exhaustive()
    }
}
fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
impl Store {
    /// SPEC §4.2: enrollment persists independently of current connectivity.
    pub fn enrolled_hosts(&self) -> Result<Vec<EnrolledHost>, StoreError> {
        let mut query = self.conn.prepare("SELECT host_id,host_name,revoked FROM enrolled_hosts ORDER BY host_name LIMIT 4097")?;
        let hosts = query.query_map([], |r| Ok(EnrolledHost { host_id: r.get(0)?, host_name: r.get(1)?, revoked: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        if hosts.len() > 4096 { return Err(StoreError::Conflict); }
        Ok(hosts)
    }
    pub fn create_host_invitation(
        &self,
        digest_value: &str,
        name: &str,
        expires: i64,
        now: i64,
    ) -> Result<(), StoreError> {
        if !digest(digest_value)
            || !valid_name(name)
            || now < 0
            || expires <= now
            || expires.checked_sub(now).is_none_or(|v| v > 3600)
        {
            return Err(StoreError::Conflict);
        }
        let tx = TransactionBehavior::Immediate;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, tx)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_name=?1)",
            [name],
            |r| r.get(0),
        )?;
        if exists {
            return Err(StoreError::Conflict);
        }
        tx.execute(
            "INSERT INTO host_invitations VALUES(?1,?2,?3)",
            params![digest_value, name, expires],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn redeem_host_invitation<F>(
        &self,
        request: &Redemption,
        now: i64,
        issue: F,
    ) -> Result<CertificateRecord, StoreError>
    where
        F: FnOnce(&str) -> Result<CertificateRecord, StoreError>,
    {
        if !digest(&request.invitation_digest)
            || !digest(&request.key_digest)
            || !digest(&request.csr_digest)
            || !valid_name(&request.host_name)
            || !valid_name(&request.transaction_id)
            || now < 0
        {
            return Err(StoreError::Conflict);
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let previous: Option<(String,String,String,String,String)> = tx.query_row("SELECT transaction_id,host_name,key_digest,csr_digest,fingerprint FROM host_enrollment_transactions WHERE invitation_digest=?1", [&request.invitation_digest], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        if let Some((transaction, name, key, csr, fingerprint)) = previous {
            if transaction != request.transaction_id
                || name != request.host_name
                || key != request.key_digest
                || csr != request.csr_digest
            {
                return Err(StoreError::IdempotencyConflict);
            }
            let record = read_certificate(&tx, &fingerprint, now)?;
            tx.commit()?;
            return Ok(record);
        }
        let invitation: Option<(String, i64)> = tx
            .query_row(
                "SELECT host_name,expires_unix FROM host_invitations WHERE digest=?1",
                [&request.invitation_digest],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if !invitation.is_some_and(|(name, expiry)| name == request.host_name && now < expiry) {
            return Err(StoreError::Conflict);
        }
        let host_id = ulid::Ulid::new().to_string();
        tx.execute(
            "INSERT INTO enrolled_hosts(host_id,host_name,key_digest) VALUES(?1,?2,?3)",
            params![host_id, request.host_name, request.key_digest],
        )?;
        let issued = issue(&host_id)?;
        if issued.host_id != host_id {
            return Err(StoreError::Conflict);
        }
        insert_certificate(&tx, &issued, now)?;
        tx.execute(
            "INSERT INTO host_enrollment_transactions VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                request.invitation_digest,
                request.transaction_id,
                request.host_name,
                request.key_digest,
                request.csr_digest,
                issued.fingerprint
            ],
        )?;
        tx.commit()?;
        Ok(issued)
    }
    pub fn certificate_host(
        &self,
        fingerprint: &str,
        now: i64,
    ) -> Result<CertificateRecord, StoreError> {
        if !digest(fingerprint) || now < 0 {
            return Err(StoreError::Conflict);
        }
        read_certificate(&self.conn, fingerprint, now)
    }
    pub fn renew_host_certificate<F>(
        &self,
        fingerprint: &str,
        key_digest: &str,
        transaction_id: &str,
        csr_digest: &str,
        now: i64,
        issue: F,
    ) -> Result<CertificateRecord, StoreError>
    where
        F: FnOnce(&str) -> Result<CertificateRecord, StoreError>,
    {
        if !digest(fingerprint)
            || !digest(key_digest)
            || !digest(csr_digest)
            || !valid_name(transaction_id)
            || now < 0
        {
            return Err(StoreError::Conflict);
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_certificate(&tx, fingerprint, now)?;
        let key: String = tx.query_row(
            "SELECT key_digest FROM enrolled_hosts WHERE host_id=?1",
            [&current.host_id],
            |r| r.get(0),
        )?;
        if key != key_digest {
            return Err(StoreError::Conflict);
        }
        let previous: Option<(String,String)> = tx.query_row("SELECT csr_digest,fingerprint FROM host_certificate_renewals WHERE host_id=?1 AND transaction_id=?2",params![current.host_id,transaction_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((csr, fingerprint)) = previous {
            if csr != csr_digest {
                return Err(StoreError::IdempotencyConflict);
            }
            let replay = read_certificate(&tx, &fingerprint, now)?;
            tx.commit()?;
            return Ok(replay);
        }
        let renewed = issue(&current.host_id)?;
        if renewed.host_id != current.host_id {
            return Err(StoreError::Conflict);
        }
        insert_certificate(&tx, &renewed, now)?;
        tx.execute(
            "INSERT INTO host_certificate_renewals VALUES(?1,?2,?3,?4)",
            params![
                current.host_id,
                transaction_id,
                csr_digest,
                renewed.fingerprint
            ],
        )?;
        tx.commit()?;
        Ok(renewed)
    }
    /// SPEC §§4.1, 13.3: revoke an enrolled host, named by its id or, failing
    /// that, by its unique name. Revocation is absorbing: revoking a revoked
    /// host changes nothing and reports `newly_revoked: false`, so a retried
    /// command is answered the same way (SPEC §6.4). The first revocation is
    /// journaled in the same transaction. Nothing the host owns is released
    /// here: its runtimes, reservations and leases stay accounted until
    /// verified evidence settles them.
    pub fn revoke_host(&self, host: &str) -> Result<Revocation, StoreError> {
        if !valid_name(host) {
            return Err(StoreError::Conflict);
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // An id is matched first, so a name can never shadow another host's id.
        let found: Option<(String, String, bool)> = match tx
            .query_row(
                "SELECT host_id,host_name,revoked FROM enrolled_hosts WHERE host_id=?1",
                [host],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
        {
            Some(found) => Some(found),
            None => tx
                .query_row(
                    "SELECT host_id,host_name,revoked FROM enrolled_hosts WHERE host_name=?1",
                    [host],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?,
        };
        let (host_id, host_name, already) = found.ok_or(StoreError::Conflict)?;
        if !already {
            tx.execute(
                "UPDATE enrolled_hosts SET revoked=1 WHERE host_id=?1",
                [&host_id],
            )?;
            crate::events::append_event(
                &tx,
                &crate::events::EventMetadata::HostRevoked {
                    host_id: host_id.clone(),
                    host_name: host_name.clone(),
                },
            )
            .map_err(|_| StoreError::Conflict)?;
        }
        tx.commit()?;
        Ok(Revocation {
            host_id,
            host_name,
            newly_revoked: !already,
        })
    }
}
fn read_certificate(
    conn: &rusqlite::Connection,
    fingerprint: &str,
    now: i64,
) -> Result<CertificateRecord, StoreError> {
    conn.query_row("SELECT c.host_id,c.fingerprint,c.certificate_pem,c.expires_unix FROM host_certificates c JOIN enrolled_hosts h ON h.host_id=c.host_id WHERE c.fingerprint=?1 AND h.revoked=0 AND c.expires_unix>?2",params![fingerprint,now], |r| Ok(CertificateRecord { host_id:r.get(0)?,fingerprint:r.get(1)?,certificate_pem:r.get(2)?,expires_unix:r.get(3)? })).optional()?.ok_or(StoreError::Conflict)
}
fn insert_certificate(
    tx: &rusqlite::Transaction<'_>,
    record: &CertificateRecord,
    now: i64,
) -> Result<(), StoreError> {
    if !digest(&record.fingerprint)
        || record.certificate_pem.is_empty()
        || record.certificate_pem.len() > 65536
        || record.expires_unix <= now
    {
        return Err(StoreError::Conflict);
    }
    tx.execute(
        "INSERT INTO host_certificates VALUES(?1,?2,?3,?4)",
        params![
            record.fingerprint,
            record.host_id,
            record.certificate_pem,
            record.expires_unix
        ],
    )?;
    Ok(())
}
