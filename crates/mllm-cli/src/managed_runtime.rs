//! SPEC §3.3 / ADR 0001 (owner decision 2026-09-24): the roles that launch
//! engines (host, standalone) run from the runtime helpers compiled into this
//! binary, written to `<state_dir>/runtime` at `init` and on every start
//! (`mllm_agent::embedded_runtime`). An operator-declared runtime directory
//! is never written; the server launches no engine and has no runtime.

use std::path::Path;

use mllm_agent::embedded_runtime::{materialize, MaterializeError, Materialized};

use crate::output::StructuredError;

/// Write or check the managed runtime directory `dir`, reporting a refresh or
/// a restore on stderr (never file contents).
pub fn prepare(dir: &Path) -> Result<Materialized, MaterializeError> {
    let outcome = materialize(dir)?;
    if let Some(notice) = outcome.notice(dir) {
        eprintln!("warning: {notice}");
    }
    Ok(outcome)
}

/// As [`prepare`], as a role error: a managed runtime that cannot be written
/// refuses the role before it serves (SPEC §13.3 / T21 T37).
pub fn prepare_for_role(dir: &Path) -> Result<Materialized, StructuredError> {
    prepare(dir).map_err(|error| StructuredError {
        code: "invalid_config",
        message: format!("managed runtime directory: {error}"),
    })
}
