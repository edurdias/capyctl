//! Embedded host supervision: the doctor's host checks and the host memory
//! observation the controller admits against.
//!
//! The embedded fake host that used to live here left with the Fake engine: a
//! host that supervises nothing real has no place in the shipped binary, and the
//! tests that drove it now inject the Fake through the engine provider
//! (`capyctl-testkit`).

// ADR 0014 §7 (WE3): checkpoint digest measured on the host.
pub mod checkpoint;
pub mod doctor;
// SPEC §3.3 / ADR 0001: the runtime helpers compiled into the one binary.
pub mod embedded_runtime;
pub mod enrollment;
// SPEC §7.2 / ADR 0019: per-device GPU memory and the host shape.
pub mod gpu_memory;
// ADR 0019: the domains a host reports and its start-time device check.
pub mod device_domains;
// ADR 0028 §7: the read-only host checks and peer address of a group member.
pub mod host_checks;
// ADR 0018 §3: the owner-only local control socket for engine add, remove
// and list.
pub mod control_socket;
// SPEC §13.2 (W13): exits of owned engine processes, reported to the controller.
pub mod exits;
pub mod identity;
pub mod identity_storage;
pub mod ingress;
pub mod ingress_identity;
// ADR 0008 (owner decision 2026-09-23): installation fingerprints and
// launch-time capability probes, instead of pinned hashes.
pub mod installation;
// ADR 0018 §1: resolving a named engine installation and its bounded
// version check.
pub mod engines;
pub mod load;
pub mod memory;
// ADR 0007: per-process resident memory, sampled beside availability.
pub mod process_residency;
// SPEC §8.2 / T21: per-launch SGLang rendezvous directories, removed on gone.
pub mod rendezvous;
// ADR 0023 §3: the private per-version TensorFold build cache.
pub mod engine_cache;
// SPEC §9.1, §13.3: capyctl's runtime directory is what this account put there.
pub mod runtime_integrity;
// ADR 0008: declared model sources materialized into the host's model store.
pub mod sources;

pub mod journal;

pub mod session;

// ADR 0018 §3: the accepted, pending and previous runtime profile sets.
pub mod profiles;
// ADR 0018 §3, §4: the host's answers to engine add, remove and list.
pub mod host_control;

pub mod native_execution;
