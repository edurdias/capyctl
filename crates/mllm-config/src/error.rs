//! Error types for strict config loading.

use thiserror::Error;

/// Machine-readable error code for a config failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConfigErrorCode {
    #[error("unknown field")]
    UnknownField,
    #[error("duplicate key")]
    DuplicateKey,
    #[error("invalid unit")]
    InvalidUnit,
    #[error("missing required field")]
    MissingRequired,
    #[error("unsupported combination")]
    UnsupportedCombination,
    #[error("conflicting args")]
    ConflictingArgs,
    #[error("invalid cache reference")]
    InvalidCacheRef,
    #[error("contradictory connection")]
    ContradictoryConnection,
    #[error("schema version / kind mismatch")]
    SchemaVersion,
    /// The named model source has no local path yet. SPEC §13.3 keeps fetching
    /// out of the resolver: only a local source names a file the host already
    /// holds, so asking a `huggingface` or `http` source where its weights are
    /// is answered with this rather than with a guessed cache location.
    #[error("model source is not materializable")]
    NotMaterializable,
    /// ADR 0008: the host's `model_sources` policy does not allow this
    /// remote source (remote sources are denied unless the host opts in).
    #[error("model source denied by host policy")]
    ModelSourceDenied,
    /// Filesystem/OS-level failures: unreadable files, missing paths
    /// (including an explicit config path that does not exist), failed
    /// atomic writes, unreadable OS entropy. Callers distinguishing
    /// "not found" from other I/O problems should inspect `detail`.
    #[error("io error")]
    Io,
}

/// A config validation failure: code plus dotted path and human detail.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code} at `{path}`: {detail}")]
pub struct ConfigError {
    pub code: ConfigErrorCode,
    pub path: String,
    pub detail: String,
}

impl ConfigError {
    pub fn new(code: ConfigErrorCode, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code,
            path: path.into(),
            detail: detail.into(),
        }
    }
}
