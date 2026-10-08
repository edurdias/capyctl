pub mod client;
// Owner decision 2026-09-25: the minimal deployment file as the CLI reads it.
mod client_journal;
pub mod deployment_file;
pub mod detail;
pub mod device_inventory;
pub mod drain;
pub mod engine;
// Design §9: the inference authentication opt-out and its warning.
pub mod exposure;
pub mod grammar;
pub mod host_observation;
// Owner decision 2026-09-26: the role running on this machine, which client
// commands use without --config.
pub mod local_role;
pub mod managed_runtime;
pub mod output;
pub mod policy_migration;
// SPEC §6.3, ADR 0008: `capyctl prune sources`.
pub mod prune;
pub mod remote_roles;
pub mod revoke;
// SPEC §§7.2, 13.2: a role reaps what it inherits and names its memory bound.
pub mod role_process;
pub mod role_text;
pub mod roles;
// Owner decision 2026-09-25: the generic `--set` / `CAPYCTL_SET__…` overrides.
pub mod settings;
pub mod shutdown;
pub mod standalone_config;
// ADR 0018 §5: standalone's engine add, remove and list.
pub mod standalone_engines;
// Owner decision 2026-09-25: table output for record views.
pub mod table;
pub mod validate;
pub mod views;
