//! ADR 0028 §5, amendment of 2026-10-07 (owner decision): the weights one
//! member of a multi-node engine group holds when its phases derive from
//! `engine_config.memory`.
//!
//! The engines split a checkpoint across ranks: tensor parallelism shards the
//! decoder layers' projections and experts across the ranks of a stage, and
//! pipeline parallelism gives each stage a contiguous run of layers. Some
//! tensors stay whole on every rank that holds them: norms, row-parallel
//! biases, MoE routers, multi-head-latent low-rank projections, and outside
//! the layers the embeddings, the output head and any other table (vLLM 0.30
//! and SGLang 0.5.21 shard the vocabulary across tensor-parallel ranks and
//! put the embeddings on the first stage and the head on the last;
//! TensorFold 0.6.5 keeps the embeddings whole on both of its ranks, and the
//! head too for some families). A member is charged
//!
//! ```text
//! replicated = weights - sharded
//! stage      = sharded                                         when pipeline_parallel = 1
//!            = min(sharded, ceil(layers / pipeline_parallel) x largest_layer)   otherwise
//! share      = ceil(stage / tensor_parallel) + replicated
//! ```
//!
//! where `sharded`, `layers` and `largest_layer` come from the checkpoint's
//! safetensors headers ([`CheckpointLayout`]): everything the headers do not
//! show split across ranks (the tensors kept whole, other weight files, a draft
//! model, the headers themselves) counts as replicated. Without the headers,
//! [`FALLBACK_REPLICATED_PERCENT`] of the weights is replicated and a stage
//! holds an even split of the rest.

use serde::Serialize;

/// The share of the weights a member reserves for tensors kept whole when the
/// checkpoint's safetensors headers could not be read (a checkpoint of other
/// files, or one measured by an older host). The embeddings and output head of
/// the models groups run are a few percent of their weights (about 3 % for a
/// 70B dense model, under 1 % for a 235B MoE), so a tenth also covers a
/// pipeline stage holding one layer more than the even split.
pub const FALLBACK_REPLICATED_PERCENT: i64 = 10;

/// What a checkpoint's safetensors headers say about how its weights split
/// across ranks, measured by the host beside the weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckpointLayout {
    /// Bytes of the tensors the engines split across ranks: the numbered
    /// decoder layers' tensors of two or more dimensions, less the MoE
    /// routers and low-rank projections every rank keeps whole.
    pub sharded_bytes: i64,
    /// The numbered decoder layers.
    pub layer_count: u32,
    /// The largest decoder layer's sharded bytes.
    pub largest_layer_bytes: i64,
}

impl CheckpointLayout {
    /// Whether every field is in range: nothing negative, no layer larger than
    /// all of them, and a layer only with layers.
    pub fn is_valid(&self) -> bool {
        self.sharded_bytes >= 0
            && (0..=self.sharded_bytes).contains(&self.largest_layer_bytes)
            && (self.layer_count > 0 || self.sharded_bytes == 0)
    }
}

/// ADR 0028 §5: what a group member's derived memory was sized with, recorded
/// so a snapshot re-derives identically. Absent for a single-host deployment
/// and for a member whose `resources` are declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MemberWeights {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
    /// The whole checkpoint's measured weights, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_weights_bytes: Option<i64>,
    /// The checkpoint's layout, when the host could read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<CheckpointLayout>,
}

/// The weights one member of a `tensor_parallel` x `pipeline_parallel` group
/// holds of a checkpoint of `weights` bytes (module documentation). `weights`
/// itself for a world of one; `None` only past `i64`.
pub fn member_weights_bytes(
    weights: i64,
    layout: Option<&CheckpointLayout>,
    tensor_parallel: u32,
    pipeline_parallel: u32,
) -> Option<i64> {
    let (tp, pp) = (i64::from(tensor_parallel), i64::from(pipeline_parallel));
    if weights <= 0 || tp < 1 || pp < 1 || tp * pp == 1 {
        return Some(weights);
    }
    let sharded = match layout {
        Some(layout) => layout.sharded_bytes.clamp(0, weights),
        None => weights - ceil_div(weights.checked_mul(FALLBACK_REPLICATED_PERCENT)?, 100),
    };
    let replicated = weights - sharded;
    let stage = match layout {
        _ if pp == 1 => sharded,
        Some(layout) if layout.layer_count > 0 => {
            let layers = ceil_div(i64::from(layout.layer_count), pp);
            layers
                .checked_mul(layout.largest_layer_bytes.max(0))
                .map_or(sharded, |bytes| bytes.min(sharded))
        }
        _ => ceil_div(sharded, pp),
    };
    ceil_div(stage, tp).checked_add(replicated)
}

fn ceil_div(value: i64, divisor: i64) -> i64 {
    value / divisor + i64::from(value % divisor != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    fn layout(sharded: i64, layers: u32, largest: i64) -> CheckpointLayout {
        CheckpointLayout {
            sharded_bytes: sharded,
            layer_count: layers,
            largest_layer_bytes: largest,
        }
    }

    #[test]
    fn a_world_of_one_holds_the_whole_checkpoint() {
        let l = layout(8 * GIB, 4, 2 * GIB);
        assert_eq!(
            member_weights_bytes(10 * GIB, Some(&l), 1, 1),
            Some(10 * GIB)
        );
        assert_eq!(member_weights_bytes(10 * GIB, None, 1, 1), Some(10 * GIB));
        assert_eq!(member_weights_bytes(0, None, 2, 1), Some(0));
    }

    #[test]
    fn tensor_parallel_splits_all_but_the_replicated_tensors() {
        // 126 GiB with 2 GiB kept whole: half of 124 GiB, and the 2 GiB.
        let l = layout(124 * GIB, 48, 3 * GIB);
        assert_eq!(
            member_weights_bytes(126 * GIB, Some(&l), 2, 1),
            Some(62 * GIB + 2 * GIB)
        );
        // An odd remainder rounds up.
        let odd = layout(3, 1, 3);
        assert_eq!(member_weights_bytes(3, Some(&odd), 2, 1), Some(2));
    }

    #[test]
    fn pipeline_stages_hold_their_largest_run_of_layers() {
        // 61 layers of at most 2 GiB: the heavier stage holds 31.
        let l = layout(61 * 2 * GIB, 61, 2 * GIB);
        let weights = 61 * 2 * GIB + GIB;
        assert_eq!(
            member_weights_bytes(weights, Some(&l), 1, 2),
            Some(31 * 2 * GIB + GIB)
        );
        // Tensor parallel splits the stage again.
        assert_eq!(
            member_weights_bytes(weights, Some(&l), 2, 2),
            Some(31 * GIB + GIB)
        );
        // Never more than every layer.
        let one = layout(10 * GIB, 1, 10 * GIB);
        assert_eq!(
            member_weights_bytes(11 * GIB, Some(&one), 1, 2),
            Some(11 * GIB)
        );
    }

    #[test]
    fn without_the_headers_a_tenth_is_kept_whole() {
        let weights = 100 * GIB;
        assert_eq!(
            member_weights_bytes(weights, None, 2, 1),
            Some(45 * GIB + 10 * GIB)
        );
        assert_eq!(
            member_weights_bytes(weights, None, 2, 2),
            Some(ceil_div(ceil_div(90 * GIB, 2), 2) + 10 * GIB)
        );
    }

    #[test]
    fn sharded_bytes_never_exceed_the_weights() {
        let l = layout(50 * GIB, 4, GIB);
        assert_eq!(
            member_weights_bytes(10 * GIB, Some(&l), 2, 1),
            Some(5 * GIB)
        );
        assert!(!layout(GIB, 2, 2 * GIB).is_valid());
        assert!(!layout(GIB, 0, 0).is_valid());
        assert!(layout(0, 0, 0).is_valid());
    }
}
