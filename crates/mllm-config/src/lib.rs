//! `mllm-config`: strict YAML config loading, schema validation, and
//! normalized JSON view production.

pub mod defaults;
pub mod error;
pub mod schema;
pub mod strict_yaml;

pub use defaults::{generate_default, resolve_startup, LoadOutcome};
pub use error::{ConfigError, ConfigErrorCode};
pub use schema::ConfigKind;
pub use strict_yaml::{parse_strict, validate};
