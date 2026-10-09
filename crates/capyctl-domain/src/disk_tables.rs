//! ADR 0014 amendment A20 (owner decision 2026-10-09): tables an engine
//! option keeps on disk count against the models disk, not memory.
//!
//! Qwen3.8-Flash-Next carries a per-layer embedding (an n-gram table) of
//! 47.7 GiB beside 78.3 GiB of other weights, so its 126 GiB checkpoint never
//! fits a 121.7 GiB GB10 when every weight byte is charged as memory. Two
//! engines can leave that table on disk:
//!
//! - SGLang 0.5.21 `--ple-offload-backend file` (with `--ple-offload-embedding`
//!   on, its default for a BF16 model on CUDA): the table is a shared mapping
//!   of a sparse file under `--ple-offload-dir`, read through the host page
//!   tables, and a trimmer keeps each table's resident set under
//!   `SGLANG_QWEN4_PLE_FILE_RSS_BUDGET_GB` (8 GiB by default, checked every
//!   30 s) (`sglang/srt/models/qwen4_exp_ple_table.py`);
//! - TensorFold 0.6.5 `--ple-on-ssd`: each lookup reads the rows from the
//!   checkpoint's own files with `pread` (readahead off) and holds no table in
//!   memory (`tensorfold/families/qwen4_exp/ssd_table.py`).
//!
//! vLLM 0.30 has no such option. Both engines read the same tensors, the
//! shards of each layer's `ple_embedding.ngram_embedding` (`.weight`, and the
//! `.scales` and `.biases` of a 4-bit table), which the host finds by name in
//! the checkpoint's safetensors headers ([`is_table_tensor`]). With the option
//! on, the memory weights are
//!
//! ```text
//! resident = weights - tables                       (a group member: its share of it)
//! memory   = resident + min(tables, count x cache)
//! ```
//!
//! where `cache` is the engine's in-memory cache of one table
//! ([`SGLANG_TABLE_CACHE_BYTES`], [`TENSORFOLD_TABLE_CACHE_BYTES`]). The disk
//! accounting is unchanged: the model store already holds the files the
//! tables are read from.

use serde::Serialize;

use crate::member_weights::{member_weights_bytes, CheckpointLayout};

/// SGLang 0.5.21's default resident-set budget for one file-backed table
/// (`SGLANG_QWEN4_PLE_FILE_RSS_BUDGET_GB`, 8 GiB): faulted rows map whole
/// page-cache folios, and the trimmer drops them once the mapping holds more.
pub const SGLANG_TABLE_CACHE_BYTES: i64 = 8 << 30;

/// TensorFold 0.6.5 keeps no table in memory with `--ple-on-ssd`: a lookup
/// reads its rows into buffers of the rows asked for (tens of MiB for a long
/// prompt chunk) through 16 reader threads, with readahead off. One GiB per
/// table covers those buffers and the page cache the reads leave behind.
pub const TENSORFOLD_TABLE_CACHE_BYTES: i64 = 1 << 30;

/// What a checkpoint's safetensors headers say about its tables, measured by
/// the host beside the weights. Absent for a checkpoint with none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckpointTables {
    /// Bytes of every table tensor.
    pub bytes: i64,
    /// The distinct tables (one per layer that has one).
    pub count: u32,
    /// Of `bytes`, what the checkpoint layout counts as split across ranks.
    pub sharded_bytes: i64,
    /// The largest decoder layer's split bytes without its tables, which a
    /// pipeline stage is sized by when the tables stay on disk.
    pub resident_largest_layer_bytes: i64,
}

impl CheckpointTables {
    /// Whether every field is in range: some table, nothing negative, and no
    /// more split bytes than bytes.
    pub fn is_valid(&self) -> bool {
        self.bytes > 0
            && self.count > 0
            && (0..=self.bytes).contains(&self.sharded_bytes)
            && self.resident_largest_layer_bytes >= 0
    }
}

/// What a deployment whose engine keeps the tables on disk was sized with,
/// recorded so a snapshot re-derives identically. Absent whenever the engine
/// option is off, so such a deployment's sizing and fingerprint are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiskTables {
    /// The whole checkpoint's measured weights, tables included.
    pub checkpoint_weights_bytes: i64,
    pub tables: CheckpointTables,
    /// The engine's in-memory cache of the tables charged back as memory.
    pub cache_bytes: i64,
}

/// Whether a tensor named `name` is a table shard both engines' options read
/// from disk: `….ple_embedding.ngram_embedding.shard_<n>.{weight,scales,biases}`
/// (SGLang 0.5.21 loads `.weight` shards; TensorFold 0.6.5 matches
/// `language_model.model.layers.<n>.ple.ple_embedding.ngram_embedding.shard_<n>.(weight|scales|biases)`).
/// The scale `ple_embedding.ngram_embedding.weight_scale` stays in memory.
pub fn is_table_tensor(name: &str) -> bool {
    table_of(name).is_some()
}

/// The table a tensor named `name` is a shard of (the name up to
/// `ngram_embedding`), or `None` when it is not a table shard.
pub fn table_of(name: &str) -> Option<&str> {
    let (table, rest) = name.rsplit_once(".shard_")?;
    let (index, part) = rest.split_once('.')?;
    (table.ends_with("ple_embedding.ngram_embedding")
        && !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
        && matches!(part, "weight" | "scales" | "biases"))
    .then_some(table)
}

/// The memory weights of a checkpoint of `weights` bytes whose `tables` stay
/// on disk, held by one member of a `tensor_parallel` x `pipeline_parallel`
/// group (a world of one for a single host), with `cache_per_table` of each
/// table cached in memory: the resident weights' share
/// ([`member_weights_bytes`] over the layout without the tables) plus the
/// cache. Returns `(memory, cache)`; `None` only past `i64`.
pub fn disk_table_weights_bytes(
    weights: i64,
    layout: Option<&CheckpointLayout>,
    tables: &CheckpointTables,
    tensor_parallel: u32,
    pipeline_parallel: u32,
    cache_per_table: i64,
) -> Option<(i64, i64)> {
    let table_bytes = tables.bytes.clamp(0, weights.max(0));
    let resident = weights.checked_sub(table_bytes)?;
    let resident_layout = layout.map(|layout| {
        let sharded = layout
            .sharded_bytes
            .saturating_sub(tables.sharded_bytes.max(0))
            .clamp(0, resident.max(0));
        CheckpointLayout {
            sharded_bytes: sharded,
            layer_count: layout.layer_count,
            largest_layer_bytes: tables.resident_largest_layer_bytes.clamp(0, sharded),
        }
    });
    let share = member_weights_bytes(
        resident,
        resident_layout.as_ref(),
        tensor_parallel,
        pipeline_parallel,
    )?;
    let cache = i64::from(tables.count)
        .checked_mul(cache_per_table.max(0))?
        .min(table_bytes);
    Some((share.checked_add(cache)?, cache))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    fn flash_next() -> CheckpointTables {
        CheckpointTables {
            bytes: 47 * GIB + 7 * GIB / 10,
            count: 1,
            sharded_bytes: 47 * GIB + 7 * GIB / 10,
            resident_largest_layer_bytes: 2 * GIB,
        }
    }

    #[test]
    fn table_shards_are_recognized_by_name() {
        for name in [
            "language_model.model.layers.3.ple.ple_embedding.ngram_embedding.shard_0.weight",
            "model.language_model.layers.3.ple.ple_embedding.ngram_embedding.shard_12.scales",
            "ple_embedding.ngram_embedding.shard_1.biases",
        ] {
            assert!(is_table_tensor(name), "{name}");
        }
        for name in [
            "language_model.model.layers.3.ple.ple_embedding.ngram_embedding.weight_scale",
            "language_model.model.layers.3.ple.ple_embedding.ngram_embedding.weight",
            "language_model.model.layers.3.ple.ple_embedding.layer_multipliers",
            "language_model.model.layers.3.ple.key_proj.weight",
            "model.embed_tokens.weight",
            "x.ngram_embedding.shard_.weight",
            "x.other.shard_0.weight",
        ] {
            assert!(!is_table_tensor(name), "{name}");
        }
        assert_eq!(
            table_of("a.layers.3.ple.ple_embedding.ngram_embedding.shard_7.weight"),
            Some("a.layers.3.ple.ple_embedding.ngram_embedding")
        );
    }

    #[test]
    fn a_single_host_holds_the_resident_weights_and_the_cache() {
        let weights = 126 * GIB;
        let tables = flash_next();
        let (memory, cache) =
            disk_table_weights_bytes(weights, None, &tables, 1, 1, SGLANG_TABLE_CACHE_BYTES)
                .unwrap();
        assert_eq!(cache, 8 * GIB);
        assert_eq!(memory, weights - tables.bytes + 8 * GIB);
        // The cache never exceeds the tables.
        let small = CheckpointTables {
            bytes: GIB,
            ..tables
        };
        assert_eq!(
            disk_table_weights_bytes(10 * GIB, None, &small, 1, 1, SGLANG_TABLE_CACHE_BYTES),
            Some((10 * GIB, GIB))
        );
    }

    #[test]
    fn a_group_member_holds_its_share_of_the_resident_weights() {
        let weights = 126 * GIB;
        let tables = flash_next();
        // 48 layers: 120 GiB split, the table among it, 6 GiB kept whole.
        let layout = CheckpointLayout {
            sharded_bytes: 120 * GIB,
            layer_count: 48,
            largest_layer_bytes: 49 * GIB,
        };
        let (memory, cache) = disk_table_weights_bytes(
            weights,
            Some(&layout),
            &tables,
            2,
            1,
            TENSORFOLD_TABLE_CACHE_BYTES,
        )
        .unwrap();
        let resident_sharded = 120 * GIB - tables.sharded_bytes;
        let replicated = (weights - tables.bytes) - resident_sharded;
        assert_eq!(cache, GIB);
        assert_eq!(memory, (resident_sharded + 1) / 2 + replicated + GIB);
        // Pipeline stages are sized by the largest layer without its table.
        let (staged, _) = disk_table_weights_bytes(
            weights,
            Some(&layout),
            &tables,
            1,
            2,
            TENSORFOLD_TABLE_CACHE_BYTES,
        )
        .unwrap();
        assert_eq!(staged, 24 * 2 * GIB + replicated + GIB);
    }

    #[test]
    fn validity() {
        assert!(flash_next().is_valid());
        assert!(!CheckpointTables {
            count: 0,
            ..flash_next()
        }
        .is_valid());
        assert!(!CheckpointTables {
            bytes: 0,
            ..flash_next()
        }
        .is_valid());
        assert!(!CheckpointTables {
            sharded_bytes: 48 * GIB,
            ..flash_next()
        }
        .is_valid());
    }
}
