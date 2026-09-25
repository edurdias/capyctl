//! ADR 0017: the release version, post-baseline capabilities and version skew
//! verdict each enrolled host declared on its latest control session.
//!
//! This is status evidence, never an authority: the live session's own
//! declaration is what gates commands. It survives a disconnect so `list
//! hosts` and deployment status can still say why an offline host needs an
//! upgrade.
use crate::{Store, StoreError};
use rusqlite::{params, OptionalExtension};

/// One host's latest declaration and verdict.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HostVersion {
    /// The host's reported release version; empty from a host that predates
    /// the policy (it is shown as such, never guessed).
    pub binary_version: String,
    /// `supported` | `upgrade_recommended` | `upgrade_required` | `refused`.
    pub compatibility: String,
    /// Empty when supported on the server's own line.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The post-baseline features the host declared.
    pub capabilities: Vec<String>,
    pub recorded_at_ms: i64,
}

const STATES: &[&str] = &[
    "supported",
    "upgrade_recommended",
    "upgrade_required",
    "refused",
];

impl Store {
    /// Record `host`'s latest declaration. The version and reason are the
    /// server's bounded, printable renderings; an unenrolled or revoked host
    /// records nothing (`Conflict`).
    pub fn record_host_version(
        &self,
        host_id: &str,
        version: &HostVersion,
    ) -> Result<(), StoreError> {
        let printable = |text: &str, bound: usize| {
            text.len() <= bound && text.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        };
        if !printable(&version.binary_version, 128)
            || !printable(&version.reason, 512)
            || !STATES.contains(&version.compatibility.as_str())
            || version.capabilities.len() > 64
            || version.recorded_at_ms < 0
        {
            return Err(StoreError::Conflict);
        }
        let capabilities =
            serde_json::to_string(&version.capabilities).map_err(|_| StoreError::Conflict)?;
        let changed = self.conn.execute(
            "INSERT INTO host_versions(host_id,binary_version,compatibility,reason,capabilities_json,recorded_at_ms)
             SELECT ?1,?2,?3,?4,?5,?6 WHERE EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1 AND revoked=0)
             ON CONFLICT(host_id) DO UPDATE SET binary_version=excluded.binary_version,
               compatibility=excluded.compatibility,reason=excluded.reason,
               capabilities_json=excluded.capabilities_json,recorded_at_ms=excluded.recorded_at_ms",
            params![
                host_id,
                version.binary_version,
                version.compatibility,
                version.reason,
                capabilities,
                version.recorded_at_ms
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// `host`'s latest recorded declaration, if it ever connected since v34.
    pub fn host_version(&self, host_id: &str) -> Result<Option<HostVersion>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT binary_version,compatibility,reason,capabilities_json,recorded_at_ms FROM host_versions WHERE host_id=?1",
                [host_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(binary_version, compatibility, reason, capabilities, recorded_at_ms)| {
                Ok(HostVersion {
                    binary_version,
                    compatibility,
                    reason,
                    capabilities: serde_json::from_str(&capabilities)
                        .map_err(|_| StoreError::Conflict)?,
                    recorded_at_ms,
                })
            },
        )
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(compatibility: &str) -> HostVersion {
        HostVersion {
            binary_version: "0.2.0".into(),
            compatibility: compatibility.into(),
            reason: String::new(),
            capabilities: vec!["heartbeats".into()],
            recorded_at_ms: 5,
        }
    }

    // T06: ADR 0017 (v34). The latest declaration replaces the previous one;
    // an unenrolled or revoked host, an unknown verdict or unprintable text
    // records nothing.
    #[test]
    fn the_latest_declaration_is_recorded_for_an_enrolled_host() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES('a','a','k',0),('r','r','k',1);",
            )
            .unwrap();
        assert!(store.host_version("a").unwrap().is_none());
        store
            .record_host_version("a", &version("supported"))
            .unwrap();
        let mut older = version("upgrade_required");
        older.binary_version = String::new();
        older.reason = "the host reports no version; it is drain-only".into();
        older.capabilities.clear();
        store.record_host_version("a", &older).unwrap();
        assert_eq!(store.host_version("a").unwrap(), Some(older));
        assert!(store
            .record_host_version("missing", &version("supported"))
            .is_err());
        assert!(store
            .record_host_version("r", &version("supported"))
            .is_err());
        assert!(store.record_host_version("a", &version("maybe")).is_err());
        let mut unprintable = version("refused");
        unprintable.reason = "line\nbreak".into();
        assert!(store.record_host_version("a", &unprintable).is_err());
        let mut long = version("supported");
        long.binary_version = "1".repeat(129);
        assert!(store.record_host_version("a", &long).is_err());
    }
}
