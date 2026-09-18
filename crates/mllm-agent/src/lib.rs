//! Embedded host supervision: the doctor's host checks and the host memory
//! observation the controller admits against.
//!
//! The embedded fake host that used to live here left with the Fake engine: a
//! host that supervises nothing real has no place in the shipped binary, and the
//! tests that drove it now inject the Fake through the engine provider
//! (`mllm-testkit`).

pub mod doctor;
pub mod memory;
