pub mod client;
mod client_journal;
pub mod device_inventory;
pub mod drain;
pub mod grammar;
pub mod host_observation;
pub mod output;
// SPEC §6.3, ADR 0008: `mllm prune sources`.
pub mod prune;
pub mod roles;
pub mod remote_roles;
pub mod revoke;
pub mod shutdown;
pub mod standalone_config;
pub mod validate;
