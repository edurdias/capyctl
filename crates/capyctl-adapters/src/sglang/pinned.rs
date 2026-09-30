//! The protected SGLang entry's descriptor contract constants.
//!
//! The single-checkpoint recipe and its checkpoint revision are gone (ADR 0014
//! §9): a deployment chooses any model and its own typed settings. The pinned
//! SGLang source audit is gone too (ADR 0008, owner decision 2026-09-23): an
//! installation is identified by the fingerprint the host records at
//! registration, and the internals capyctl hooks are probed by shape at launch.
//! One value remains: the contract names the public descriptor shape
//! `runtime/sglang_entry.py` validates. The former `source_revision` token,
//! which neither identified nor constrained the installed build, is gone from
//! the descriptor. The contract is not evidence that anything works
//! (AGENTS.md: Fake tests are not qualification).

/// The public descriptor contract (`schema_version` 2 in the entry).
pub const NATIVE_SGLANG_CONTRACT: &str = "sglang_engine_config_v2";
