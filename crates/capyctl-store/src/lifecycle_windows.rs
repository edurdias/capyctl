//! Owner decision 2026-09-22 (1), ADR 0014 amendment A1: the lifecycle windows
//! a caller asks for when it names none.
//!
//! Every Initialize and Stop is accepted only with a deadline no further out
//! than the revision's request deadline (SPEC §6; `accept_start`, the cleanup
//! acceptors). A caller that names no deadline therefore takes the
//! deployment's resolved Initialize timeout, and the fixed Stop window, both
//! lowered to the smallest request deadline any host resolved the current
//! revision with, so the store admits them on whichever host is chosen.

use crate::{Store, StoreError};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// What status shows and what callers use, in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LifecycleWindows {
    /// The smallest request deadline of the current revision on any host.
    pub request_deadline_ms: i64,
    /// The Initialize window a start that names none is given.
    pub initialize_ms: i64,
    /// The wake timeout, absent for a revision resolved before timeouts existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_ms: Option<i64>,
    /// The Stop window a stop that names none is given.
    pub stop_ms: i64,
    /// T14: `declared` or `derived` per field, from the revision's effective
    /// configuration; empty for a revision resolved before timeouts existed.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub provenance: std::collections::BTreeMap<String, String>,
}

fn integer(column: &str, path: &str) -> String {
    format!("CASE WHEN json_valid({column}) AND json_type({column},'{path}')='integer' THEN json_extract({column},'{path}') END")
}

fn text(column: &str, path: &str) -> String {
    format!("CASE WHEN json_valid({column}) AND json_type({column},'{path}')='text' THEN substr(json_extract({column},'{path}'),1,16) END")
}

/// The current revision's windows, or `None` for an unknown deployment or one
/// whose revision has no readable request deadline.
pub(crate) fn read(
    conn: &Connection,
    deployment: &str,
) -> rusqlite::Result<Option<LifecycleWindows>> {
    let e = "e.effective_json";
    let h = "h.effective_json";
    let sql = format!(
        "SELECT {},{},{},{},{},\
         (SELECT MIN({}) FROM host_effective_revisions h WHERE h.deployment_id=d.id AND h.revision=d.revision AND h.outcome='resolved'),\
         (SELECT MIN({}) FROM host_effective_revisions h WHERE h.deployment_id=d.id AND h.revision=d.revision AND h.outcome='resolved') \
         FROM deployments d JOIN effective_revisions e ON e.deployment_id=d.id AND e.revision=d.revision WHERE d.id=?1",
        integer(e, "$.request_deadline_ms"),
        integer(e, "$.timeouts.initialize_ms"),
        integer(e, "$.timeouts.wake_ms"),
        text(e, "$.timeouts.provenance.initialize"),
        text(e, "$.timeouts.provenance.wake"),
        integer(h, "$.request_deadline_ms"),
        integer(h, "$.timeouts.initialize_ms"),
    );
    type Row = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    );
    let row: Option<Row> = conn
        .query_row(&sql, params![deployment], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .optional()?;
    let Some((
        deadline,
        initialize,
        wake,
        initialize_source,
        wake_source,
        host_deadline,
        host_initialize,
    )) = row
    else {
        return Ok(None);
    };
    let Some(deadline) = deadline.filter(|d| *d > 0) else {
        return Ok(None);
    };
    let request_deadline_ms = host_deadline
        .filter(|d| *d > 0)
        .map_or(deadline, |h| h.min(deadline));
    let initialize = match (initialize, host_initialize) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let (initialize_ms, stop_ms) =
        capyctl_config::effective::lifecycle_windows(request_deadline_ms, initialize);
    let provenance = [("initialize", initialize_source), ("wake", wake_source)]
        .into_iter()
        .filter_map(|(field, source)| source.map(|s| (field.to_owned(), s)))
        .collect();
    Ok(Some(LifecycleWindows {
        request_deadline_ms,
        initialize_ms,
        wake_ms: wake.map(|w| w.min(request_deadline_ms)),
        stop_ms,
        provenance,
    }))
}

impl Store {
    /// ADR 0014 amendment A1: the windows a start or stop that names no
    /// deadline is given, for the deployment's current revision.
    pub fn lifecycle_windows(
        &self,
        deployment: &str,
    ) -> Result<Option<LifecycleWindows>, StoreError> {
        Ok(read(&self.conn, deployment)?)
    }
}
