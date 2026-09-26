pub mod client;
mod client_journal;
pub mod device_inventory;
pub mod drain;
pub mod engine;
// Design §9: the inference authentication opt-out and its warning.
pub mod exposure;
pub mod grammar;
pub mod host_observation;
pub mod managed_runtime;
pub mod output;
// SPEC §6.3, ADR 0008: `mllm prune sources`.
pub mod prune;
pub mod remote_roles;
pub mod revoke;
pub mod roles;
pub mod shutdown;
pub mod standalone_config;
// ADR 0018 §5: standalone's engine add, remove and list.
pub mod standalone_engines;
// Owner decision 2026-09-25: table output for record views.
pub mod table;
pub mod validate;
