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
/// ADR 0016: why a recovery invitation was refused. Kept apart from
/// `StoreError` so the management API tells an unknown host (404) from one
/// that is not revoked (409) and from a store failure (500).
#[derive(Debug, thiserror::Error)]
pub enum RecoveryInvitationError {
    #[error("invalid recovery invitation")]
    Invalid,
    #[error("host not found")]
    NotFound,
    /// Recovery re-enrolls a revoked identity only; an active host renews.
    #[error("host is not revoked")]
    NotRevoked,
    #[error(transparent)]
    Store(#[from] StoreError),
}
impl From<rusqlite::Error> for RecoveryInvitationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
/// ADR 0016: the enrolled identity a recovery invitation re-enrolls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryTarget {
    pub host_id: String,
    pub host_name: String,
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
    /// ADR 0016 (owner decision 2026-09-24): a single-use, short-lived
    /// invitation for the revoked host named by `host` (its id first, then its
    /// unique name) to re-enroll under the same host id. Only a revoked host
    /// may be recovered: an active one renews its certificate, and a new host
    /// takes an ordinary invitation, whose name-collision rule is unchanged.
    /// The invitation is journaled in the same transaction.
    pub fn create_host_recovery_invitation(
        &self,
        digest_value: &str,
        host: &str,
        expires: i64,
        now: i64,
    ) -> Result<RecoveryTarget, RecoveryInvitationError> {
        if !digest(digest_value)
            || !valid_name(host)
            || now < 0
            || expires <= now
            || expires.checked_sub(now).is_none_or(|v| v > 3600)
        {
            return Err(RecoveryInvitationError::Invalid);
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (host_id, host_name, revoked) =
            resolve_host(&tx, host)?.ok_or(RecoveryInvitationError::NotFound)?;
        if !revoked {
            return Err(RecoveryInvitationError::NotRevoked);
        }
        tx.execute(
            "INSERT INTO host_invitations VALUES(?1,?2,?3)",
            params![digest_value, host_name, expires],
        )?;
        tx.execute(
            "INSERT INTO host_recovery_invitations(digest,host_id) VALUES(?1,?2)",
            params![digest_value, host_id],
        )?;
        crate::events::append_event(
            &tx,
            &crate::events::EventMetadata::HostRecoveryInvited {
                host_id: host_id.clone(),
                host_name: host_name.clone(),
                expires_unix: expires,
            },
        )
        .map_err(|_| RecoveryInvitationError::Store(StoreError::Conflict))?;
        tx.commit()?;
        Ok(RecoveryTarget { host_id, host_name })
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
        // ADR 0016: a recovery invitation re-enrolls exactly the host it names
        // and never creates another one.
        let recovering: Option<String> = tx
            .query_row(
                "SELECT host_id FROM host_recovery_invitations WHERE digest=?1",
                [&request.invitation_digest],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(host_id) = recovering {
            let issued = recover(&tx, request, &host_id, now, issue)?;
            tx.commit()?;
            return Ok(issued);
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
    /// SPEC §4.1, ADR 0016: whether the certificate with `fingerprint` was
    /// revoked, by its own fingerprint or with its host. Only a certificate
    /// this controller issued can be revoked; an unknown fingerprint is not.
    pub fn certificate_revoked(&self, fingerprint: &str) -> Result<bool, StoreError> {
        if !digest(fingerprint) {
            return Err(StoreError::Conflict);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM revoked_host_certificates WHERE fingerprint=?1)
                 OR EXISTS(SELECT 1 FROM host_certificates c JOIN enrolled_hosts h ON h.host_id=c.host_id
                           WHERE c.fingerprint=?1 AND h.revoked=1)",
            [fingerprint],
            |r| r.get(0),
        )?)
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
        let (host_id, host_name, already) = resolve_host(&tx, host)?.ok_or(StoreError::Conflict)?;
        if !already {
            tx.execute(
                "UPDATE enrolled_hosts SET revoked=1 WHERE host_id=?1",
                [&host_id],
            )?;
            // ADR 0016: every certificate the host holds is revoked by its
            // own fingerprint, so a later recovery (a new certificate for the
            // same host id) never makes one of them valid again.
            revoke_certificates(&tx, &host_id)?;
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
    conn.query_row("SELECT c.host_id,c.fingerprint,c.certificate_pem,c.expires_unix FROM host_certificates c JOIN enrolled_hosts h ON h.host_id=c.host_id WHERE c.fingerprint=?1 AND h.revoked=0 AND c.expires_unix>?2 AND NOT EXISTS(SELECT 1 FROM revoked_host_certificates r WHERE r.fingerprint=c.fingerprint)",params![fingerprint,now], |r| Ok(CertificateRecord { host_id:r.get(0)?,fingerprint:r.get(1)?,certificate_pem:r.get(2)?,expires_unix:r.get(3)? })).optional()?.ok_or(StoreError::Conflict)
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

/// SPEC §§4.1, 13.3: the enrolled host `host` names: an id is matched first,
/// so a name can never shadow another host's id.
fn resolve_host(
    tx: &rusqlite::Transaction<'_>,
    host: &str,
) -> Result<Option<(String, String, bool)>, rusqlite::Error> {
    let by = |column: &str| {
        tx.query_row(
            &format!("SELECT host_id,host_name,revoked FROM enrolled_hosts WHERE {column}=?1"),
            [host],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
    };
    match by("host_id")? {
        Some(found) => Ok(Some(found)),
        None => by("host_name"),
    }
}
/// ADR 0016: revoke, by fingerprint, every certificate `host_id` holds.
fn revoke_certificates(tx: &rusqlite::Transaction<'_>, host_id: &str) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT OR IGNORE INTO revoked_host_certificates(fingerprint,revoked_at_unix)
           SELECT fingerprint,MAX(CAST(strftime('%s','now') AS INTEGER),0) FROM host_certificates WHERE host_id=?1",
        [host_id],
    )?;
    Ok(())
}
/// ADR 0016 (owner decision 2026-09-24): redeem a recovery invitation. The
/// host it names must still be revoked and still carry the invited name; it
/// gets a new certificate bound to the same host id, for the new key, and
/// every older certificate stays revoked. The revocation lifts in the same
/// transaction and the recovery is journaled. Nothing the host owns is
/// released or re-proven here: its engines reopen only on fresh evidence.
fn recover<F>(
    tx: &rusqlite::Transaction<'_>,
    request: &Redemption,
    host_id: &str,
    now: i64,
    issue: F,
) -> Result<CertificateRecord, StoreError>
where
    F: FnOnce(&str) -> Result<CertificateRecord, StoreError>,
{
    let (name, revoked): (String, bool) = tx
        .query_row(
            "SELECT host_name,revoked FROM enrolled_hosts WHERE host_id=?1",
            [host_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or(StoreError::Conflict)?;
    if !revoked || name != request.host_name {
        return Err(StoreError::Conflict);
    }
    revoke_certificates(tx, host_id)?;
    let issued = issue(host_id)?;
    if issued.host_id != host_id {
        return Err(StoreError::Conflict);
    }
    insert_certificate(tx, &issued, now)?;
    tx.execute(
        "UPDATE enrolled_hosts SET revoked=0,key_digest=?2 WHERE host_id=?1 AND revoked=1",
        params![host_id, request.key_digest],
    )?;
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
    crate::events::append_event(
        tx,
        &crate::events::EventMetadata::HostRecovered {
            host_id: host_id.to_owned(),
            host_name: name,
        },
    )
    .map_err(|_| StoreError::Conflict)?;
    Ok(issued)
}
