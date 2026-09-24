//! Embedded host supervision: the doctor's host checks and the host memory
//! observation the controller admits against.
//!
//! The embedded fake host that used to live here left with the Fake engine: a
//! host that supervises nothing real has no place in the shipped binary, and the
//! tests that drove it now inject the Fake through the engine provider
//! (`mllm-testkit`).

// ADR 0014 §7 (WE3): checkpoint digest measured on the host.
pub mod checkpoint;
pub mod doctor;
pub mod enrollment;
// SPEC §13.2 (W13): exits of owned engine processes, reported to the controller.
pub mod exits;
pub mod identity;
pub mod identity_storage;
pub mod ingress;
pub mod ingress_identity;
// ADR 0008 (owner decision 2026-09-23): installation fingerprints and
// launch-time capability probes, instead of pinned hashes.
pub mod installation;
pub mod load;
pub mod memory;
// ADR 0007: per-process resident memory, sampled beside availability.
pub mod process_residency;
// SPEC §8.2 / T21: per-launch SGLang rendezvous directories, removed on gone.
pub mod rendezvous;
// SPEC §9.1, §13.3: mllm's runtime directory is what this account put there.
pub mod runtime_integrity;
// ADR 0008: declared model sources materialized into the host's model store.
pub mod sources;

pub mod journal;

pub mod session;

pub mod native_execution;
