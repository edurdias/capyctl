//! `mllm-config`: strict YAML config loading, schema validation, and
//! normalized JSON view production.

pub mod defaults;
pub mod effective;
pub mod engine_policy;
pub mod error;
pub mod instances;
pub mod resource_controls;
pub mod remote_roles;
pub mod remote_resources;
pub mod schema;
// SPEC §15.3: standalone values the role does not honour are refused.
pub mod standalone;
pub mod strict_yaml;

pub use defaults::{generate_default, resolve_startup, LoadOutcome};
pub use error::{ConfigError, ConfigErrorCode};
pub use schema::ConfigKind;
pub use strict_yaml::{parse_strict, validate};
