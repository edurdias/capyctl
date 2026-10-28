//! ADR 0036 §5: the readiness probe of an embeddings deployment, one fixed
//! embedding request sent to the launch's own loopback endpoint with its own
//! key, never through ingress or the router. SPEC §6.1: `/health` and
//! `/v1/models` alone are never readiness, and an embeddings deployment is
//! never sent a chat probe in place of this one.

/// The engine route the probe and every embeddings request use.
pub const PATH: &str = "/v1/embeddings";

/// ADR 0036 §5-§6: the probe's kind and version, recorded with the wake
/// canary reference. Any change to the request is a new version, and a
/// reference recorded by another kind or version is never compared.
pub const KIND: &str = "embedding";
pub const VERSION: u32 = 1;

/// ADR 0036 §5: the fixed input of version 1.
pub const INPUT: &str = "CapyCTL readiness check: a quiet library with tall shelves of old books.";

/// ADR 0036 §5: a well-formed probe answer: one finite vector whose
/// Euclidean norm is finite and above 1e-6. Its length is the instance's
/// native dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingAnswer {
    pub vector: Vec<f32>,
}

impl EmbeddingAnswer {
    /// The native dimension of the instance that answered.
    pub fn dimension(&self) -> usize {
        self.vector.len()
    }
}
