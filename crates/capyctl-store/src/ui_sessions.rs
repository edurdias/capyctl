//! ADR 0040 §§4, 5, 7: browser sessions and one-time sign-in links.
//!
//! Only SHA-256 digests of the 256-bit random tokens enter SQLite; a token
//! never reaches the store. Each session and link records the fingerprint of
//! the admin token's digest it was issued under. A role started with another
//! admin token deletes every session and link whose fingerprint differs
//! (`sweep_ui_credentials`), and consuming a link checks it, so rotating the
//! admin token by restart ends every session and every outstanding link
//! (§7). Times are milliseconds since the Unix epoch, passed in by the caller.
use crate::{Store, StoreError};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

/// A SHA-256 digest: of a session or sign-in link token, or (as a
/// fingerprint) of the admin token's digest.
pub type Digest = [u8; 32];

/// ADR 0040 §4: the most characters a session's label holds; the caller
/// shortens the user agent to fit.
pub const UI_SESSION_LABEL_MAX: usize = 128;

/// ADR 0040 §4: last-seen is written at most once in this window.
pub const LAST_SEEN_INTERVAL_MS: i64 = 60_000;

/// A browser session, never carrying its token digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSession {
    /// A random ULID, never the digest.
    pub id: String,
    /// A short name derived from the browser's user agent.
    pub label: String,
    pub created_at_ms: i64,
    pub last_seen_at_ms: i64,
    /// When the session cookie was last issued (ADR 0040 §4).
    pub cookie_refreshed_at_ms: i64,
}

/// ADR 0040 §5: what consuming a sign-in link did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SigninOutcome {
    /// The link was spent and this session created.
    SignedIn { session_id: String },
    /// The link was spent before; no session was created.
    Used,
    /// The link is past its expiry, or was issued under another admin token
    /// (§7); no session was created.
    Expired,
    /// No such link is stored.
    Unknown,
}

impl Store {
    /// ADR 0040 §5: store a sign-in link by its token's digest, valid until
    /// `expires_at_ms`. Several may be outstanding at once.
    pub fn issue_ui_signin_link(
        &self,
        link: &Digest,
        admin_fingerprint: &Digest,
        expires_at_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO ui_signin_links(token_digest,admin_fingerprint,expires_at_ms,consumed_at_ms)
             VALUES(?1,?2,?3,NULL)",
            params![&link[..], &admin_fingerprint[..], expires_at_ms],
        )?;
        Ok(())
    }

    /// ADR 0040 §§4, 5, 7: spend the sign-in link `link` and create the
    /// session whose token digest is `session`, in one `IMMEDIATE`
    /// transaction, so a link signs in at most one browser however many
    /// requests race for it. A link issued under an admin fingerprint other
    /// than `admin_fingerprint` reads as expired.
    pub fn consume_ui_signin_link(
        &self,
        link: &Digest,
        session: &Digest,
        admin_fingerprint: &Digest,
        label: &str,
        now_ms: i64,
    ) -> Result<SigninOutcome, StoreError> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let stored: Option<(Vec<u8>, i64, Option<i64>)> = tx
            .query_row(
                "SELECT admin_fingerprint,expires_at_ms,consumed_at_ms FROM ui_signin_links
                 WHERE token_digest=?1",
                [&link[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((fingerprint, expires_at_ms, consumed_at_ms)) = stored else {
            return Ok(SigninOutcome::Unknown);
        };
        if consumed_at_ms.is_some() {
            return Ok(SigninOutcome::Used);
        }
        if fingerprint != admin_fingerprint[..] || now_ms >= expires_at_ms {
            return Ok(SigninOutcome::Expired);
        }
        tx.execute(
            "UPDATE ui_signin_links SET consumed_at_ms=?2 WHERE token_digest=?1",
            params![&link[..], now_ms],
        )?;
        let session_id = ulid::Ulid::new().to_string();
        tx.execute(
            "INSERT INTO ui_sessions(session_id,token_digest,admin_fingerprint,label,
               created_at_ms,last_seen_at_ms,cookie_refreshed_at_ms)
             VALUES(?1,?2,?3,?4,?5,?5,?5)",
            params![
                session_id,
                &session[..],
                &admin_fingerprint[..],
                label,
                now_ms
            ],
        )?;
        tx.commit()?;
        Ok(SigninOutcome::SignedIn { session_id })
    }

    /// ADR 0040 §4: the session whose token digest is `session`, if any.
    pub fn ui_session(&self, session: &Digest) -> Result<Option<UiSession>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id,label,created_at_ms,last_seen_at_ms,cookie_refreshed_at_ms
                 FROM ui_sessions WHERE token_digest=?1",
                [&session[..]],
                read_session,
            )
            .optional()?)
    }

    /// ADR 0040 §4: record that session `id` was seen at `now_ms`, at most once
    /// per [`LAST_SEEN_INTERVAL_MS`]. Returns whether last-seen was written.
    pub fn record_ui_session_seen(&self, id: &str, now_ms: i64) -> Result<bool, StoreError> {
        let written = self.conn.execute(
            "UPDATE ui_sessions SET last_seen_at_ms=?2
             WHERE session_id=?1 AND ?2-last_seen_at_ms>=?3",
            params![id, now_ms, LAST_SEEN_INTERVAL_MS],
        )?;
        Ok(written == 1)
    }

    /// ADR 0040 §4: record that session `id`'s cookie was re-issued at
    /// `now_ms`. Returns whether the session exists.
    pub fn record_ui_session_cookie_refresh(
        &self,
        id: &str,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let written = self.conn.execute(
            "UPDATE ui_sessions SET cookie_refreshed_at_ms=?2 WHERE session_id=?1",
            params![id, now_ms],
        )?;
        Ok(written == 1)
    }

    /// ADR 0040 §6: every session, oldest first, without digests.
    pub fn ui_sessions(&self) -> Result<Vec<UiSession>, StoreError> {
        let mut query = self.conn.prepare(
            "SELECT session_id,label,created_at_ms,last_seen_at_ms,cookie_refreshed_at_ms
             FROM ui_sessions ORDER BY created_at_ms,session_id",
        )?;
        let sessions = query
            .query_map([], read_session)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    /// ADR 0040 §§4, 6: revoking a session deletes its row. Returns whether
    /// session `id` existed.
    pub fn revoke_ui_session(&self, id: &str) -> Result<bool, StoreError> {
        let deleted = self
            .conn
            .execute("DELETE FROM ui_sessions WHERE session_id=?1", [id])?;
        Ok(deleted == 1)
    }

    /// ADR 0040 §6: delete every session. Returns how many there were.
    pub fn revoke_all_ui_sessions(&self) -> Result<usize, StoreError> {
        Ok(self.conn.execute("DELETE FROM ui_sessions", [])?)
    }

    /// ADR 0040 §7: at role start, delete every session and sign-in link
    /// issued under an admin fingerprint other than `admin_fingerprint`.
    pub fn sweep_ui_credentials(&self, admin_fingerprint: &Digest) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM ui_sessions WHERE admin_fingerprint!=?1",
            [&admin_fingerprint[..]],
        )?;
        tx.execute(
            "DELETE FROM ui_signin_links WHERE admin_fingerprint!=?1",
            [&admin_fingerprint[..]],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// ADR 0040 §5: delete every sign-in link expired at `now_ms`, spent or
    /// not. A link spent within its lifetime stays until then, so a second
    /// use reads as used. Returns how many were deleted.
    pub fn prune_ui_signin_links(&self, now_ms: i64) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "DELETE FROM ui_signin_links WHERE expires_at_ms<=?1",
            [now_ms],
        )?)
    }
}

fn read_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<UiSession> {
    Ok(UiSession {
        id: r.get(0)?,
        label: r.get(1)?,
        created_at_ms: r.get(2)?,
        last_seen_at_ms: r.get(3)?,
        cookie_refreshed_at_ms: r.get(4)?,
    })
}
