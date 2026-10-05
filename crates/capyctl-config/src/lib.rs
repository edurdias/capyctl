//! `capyctl-config`: strict YAML config loading, schema validation, and
//! normalized JSON view production.

// ADR 0014 §5 (owner decision 2026-09-25): context fitted to the KV grant.
pub mod context_fit;
pub mod defaults;
// Owner decision 2026-09-25 (ADR 0014 amendment): the minimal deployment file
// and the defaults that complete it, shared by every role.
pub mod deployment_defaults;
pub mod effective;
pub mod engine_env;
pub mod engine_policy;
// Owner rule 2026-09-25: the engine-installation settings of a role, three
// ways (flag > environment > YAML > default), shared by host and standalone.
pub mod engine_settings;
pub mod error;
pub mod instances;
// ADR 0019, design §9: the one-time move of the old loopback inference bind.
// ADR 0008: declared model sources and the host's model-source policy.
pub mod model_source;
// Owner decisions 2026-09-25: the models directory and the model-source
// switches, resolved once for every role (flag > environment > YAML > default).
pub mod model_settings;
// ADR 0024: tool-call and reasoning parsers chosen by model family.
pub mod parsers;
// ADR 0018 §2: `engines.yaml`, the capyctl-owned engines file beside the role
// document, its lock, atomic write, and the merge into the host document.
pub mod registration;
pub mod remote_resources;
pub mod remote_roles;
pub mod resource_controls;
pub mod schema;
// One GPU figure and one RAM figure for the five phases of a model that restarts.
pub mod short_resources;
// Owner decision 2026-09-25: every YAML setting of a role three ways, through
// the generic `--set path=value` and `CAPYCTL_SET__PATH=value` overrides.
pub mod setting_overrides;
// SPEC §15.3: standalone values the role does not honour are refused.
pub mod standalone;
pub mod strict_yaml;
pub mod yaml_emit;
// ADR 0023 §2: TensorFold's build toolchain on the closed launch PATH.
pub mod toolchain;
// Tests across the workspace write executables through this.
#[doc(hidden)]
pub mod test_support;

pub use defaults::{generate_default, resolve_startup, LoadOutcome};
pub use error::{ConfigError, ConfigErrorCode};
pub use schema::ConfigKind;
pub use strict_yaml::{parse_document, parse_strict, parse_strict_value, validate};
