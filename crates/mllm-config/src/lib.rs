//! `mllm-config`: strict YAML config loading, schema validation, and
//! normalized JSON view production.

// ADR 0014 §5 (owner decision 2026-09-25): context fitted to the KV grant.
pub mod context_fit;
pub mod defaults;
// Owner decision 2026-09-25 (ADR 0014 amendment): the minimal deployment file
// and the defaults that complete it, shared by every role.
pub mod deployment_defaults;
pub mod effective;
pub mod engine_policy;
pub mod error;
pub mod instances;
// ADR 0019, design §9: the one-time move of the old loopback inference bind.
pub mod listener_migration;
// ADR 0008: declared model sources and the host's model-source policy.
pub mod model_source;
// Owner decisions 2026-09-25: the models directory and the model-source
// switches, resolved once for every role (flag > environment > YAML > default).
pub mod model_settings;
// ADR 0018 §2: `engines.yaml`, the mllm-owned engines file beside the role
// document, its lock, atomic write, and the merge into the host document.
pub mod registration;
pub mod remote_resources;
pub mod remote_roles;
pub mod resource_controls;
pub mod schema;
// SPEC §15.3: standalone values the role does not honour are refused.
pub mod standalone;
pub mod strict_yaml;

pub use defaults::{generate_default, resolve_startup, LoadOutcome};
pub use error::{ConfigError, ConfigErrorCode};
pub use schema::ConfigKind;
pub use strict_yaml::{parse_document, parse_strict, parse_strict_value, validate};
