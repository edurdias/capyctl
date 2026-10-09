//! ADR 0028 §5 (amendment of 2026-10-07): how a checkpoint's weights split
//! across the ranks of a group, read from its safetensors headers.
//!
//! A safetensors file starts with an 8-byte little-endian header length and a
//! JSON header naming every tensor with its dtype, shape and byte range, so
//! the layout is read without touching the tensor data. What the engines
//! split is classed by name and shape (vLLM 0.30, SGLang 0.5.21 and
//! TensorFold 0.6.5 read alike; ADR 0028 §5 cites the sources):
//!
//! - inside a numbered decoder layer (`layers.N.`, `h.N.`, `blocks.N.`),
//!   projections and experts of two or more dimensions are sharded across
//!   the tensor-parallel ranks of the stage that holds the layer;
//! - kept whole on every rank: tensors of at most one dimension (norms,
//!   biases, quantization scales, `A_log`, `dt_bias`), MoE routers
//!   (`mlp.gate.weight`, `shared_expert_gate`) and the low-rank and indexer
//!   projections of latent attention (`q_a_proj`, `kv_a_proj_with_mqa`,
//!   `indexer`, `f_a_proj`, `g_a_proj`);
//! - everything outside the layers (embeddings, output head, final norm,
//!   n-gram tables, multimodal towers and projectors) counts as whole, as
//!   does any weight file that is not safetensors.
//!
//! Counting the vocabulary tensors whole is conservative for vLLM and SGLang,
//! which shard them, and exact for TensorFold, which keeps the embeddings whole.
//! When a model has fewer key-value heads than tensor-parallel ranks, vLLM and
//! SGLang replicate its key and value projections; that case is not modeled.
//!
//! ADR 0014 amendment A20 (owner decision 2026-10-09): the same pass finds the
//! tables an engine option can keep on disk (`disk_tables::is_table_tensor`),
//! with their bytes, how many there are, what of them the layout splits, and
//! the largest layer without them.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

pub use capyctl_domain::disk_tables::CheckpointTables;
pub use capyctl_domain::member_weights::CheckpointLayout;
use serde_json::Value;

/// The largest header read: the safetensors format caps it at 100 MB.
const MAX_HEADER_BYTES: u64 = 100_000_000;

/// The checkpoint's layout from the safetensors files at `root` and one
/// directory level below (as the launch sizes its weights); `None` when it
/// has none or one cannot be read.
pub fn read_checkpoint_layout(root: &Path) -> Option<CheckpointLayout> {
    read_header_facts(root).0
}

/// ADR 0014 amendment A20: the checkpoint's tables, from the same headers as
/// its layout; `None` when it has none or a header cannot be read.
pub fn read_checkpoint_tables(root: &Path) -> Option<CheckpointTables> {
    read_header_facts(root).1
}

/// The layout and the tables from one read of the headers.
pub fn read_header_facts(root: &Path) -> (Option<CheckpointLayout>, Option<CheckpointTables>) {
    scan(root).unwrap_or((None, None))
}

fn scan(root: &Path) -> Option<(Option<CheckpointLayout>, Option<CheckpointTables>)> {
    let mut files = Vec::new();
    collect(root, 1, &mut files)?;
    if files.is_empty() {
        return None;
    }
    files.sort();
    let mut layers: BTreeMap<String, i64> = BTreeMap::new();
    let mut resident_layers: BTreeMap<String, i64> = BTreeMap::new();
    let mut tables: BTreeSet<String> = BTreeSet::new();
    let (mut sharded, mut table_bytes, mut table_sharded) = (0i64, 0i64, 0i64);
    for file in files {
        for (name, bytes, dims) in tensors(&read_header(&file)?)? {
            let table = capyctl_domain::disk_tables::table_of(name);
            if let Some(table) = table {
                table_bytes = table_bytes.checked_add(bytes)?;
                if !tables.contains(table) {
                    tables.insert(table.to_owned());
                }
            }
            let Some(layer) = sharded_layer(name, dims) else {
                continue;
            };
            sharded = sharded.checked_add(bytes)?;
            let total = layers.entry(layer.clone()).or_default();
            *total = total.checked_add(bytes)?;
            let resident = resident_layers.entry(layer).or_default();
            if table.is_some() {
                table_sharded = table_sharded.checked_add(bytes)?;
            } else {
                *resident = resident.checked_add(bytes)?;
            }
        }
    }
    let layout = CheckpointLayout {
        sharded_bytes: sharded,
        layer_count: u32::try_from(layers.len()).ok()?,
        largest_layer_bytes: layers.values().copied().max().unwrap_or(0),
    };
    let tables = CheckpointTables {
        bytes: table_bytes,
        count: u32::try_from(tables.len()).ok()?,
        sharded_bytes: table_sharded,
        resident_largest_layer_bytes: resident_layers.values().copied().max().unwrap_or(0),
    };
    Some((
        layout.is_valid().then_some(layout),
        tables.is_valid().then_some(tables),
    ))
}

fn collect(dir: &Path, depth: u8, files: &mut Vec<std::path::PathBuf>) -> Option<()> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        let metadata = std::fs::metadata(&path).ok()?;
        if metadata.is_dir() {
            if depth > 0 {
                collect(&path, depth - 1, files)?;
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("safetensors") {
            files.push(path);
        }
    }
    Some(())
}

fn read_header(path: &Path) -> Option<Value> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut length = [0u8; 8];
    file.read_exact(&mut length).ok()?;
    let length = u64::from_le_bytes(length);
    if length > MAX_HEADER_BYTES {
        return None;
    }
    let mut header = Vec::with_capacity(usize::try_from(length).ok()?);
    file.take(length).read_to_end(&mut header).ok()?;
    if header.len() as u64 != length {
        return None;
    }
    serde_json::from_slice(&header).ok()
}

/// Every tensor of one header with its name, bytes and dimensions; `None` for
/// a header that is not a safetensors one.
fn tensors(header: &Value) -> Option<Vec<(&str, i64, usize)>> {
    let mut out = Vec::new();
    for (name, tensor) in header.as_object()? {
        if name == "__metadata__" {
            continue;
        }
        let offsets = tensor.get("data_offsets")?.as_array()?;
        let (Some(start), Some(end)) = (offsets.first()?.as_i64(), offsets.get(1)?.as_i64()) else {
            return None;
        };
        let bytes = end.checked_sub(start).filter(|bytes| *bytes >= 0)?;
        let dims = tensor.get("shape")?.as_array()?.len();
        out.push((name.as_str(), bytes, dims));
    }
    Some(out)
}

/// The decoder layer a tensor named `name` of `dims` dimensions is a shard
/// of, or `None` when every rank keeps it whole.
pub fn sharded_layer(name: &str, dims: usize) -> Option<String> {
    const MULTIMODAL: &[&str] = &["vision", "visual", "audio", "image", "mm_projector"];
    const WHOLE: &[&str] = &[
        ".gate.weight",
        "shared_expert_gate",
        "q_a_proj",
        "kv_a_proj_with_mqa",
        ".indexer.",
        "f_a_proj",
        "g_a_proj",
    ];
    if dims < 2
        || MULTIMODAL.iter().any(|part| name.contains(part))
        || WHOLE.iter().any(|part| name.contains(part))
    {
        return None;
    }
    let parts: Vec<&str> = name.split('.').collect();
    parts.windows(2).enumerate().find_map(|(at, pair)| {
        (matches!(pair[0], "layers" | "h" | "blocks")
            && !pair[1].is_empty()
            && pair[1].bytes().all(|b| b.is_ascii_digit()))
        .then(|| parts[..at + 2].join("."))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, file: &str, tensors: &[(&str, &[u64], i64)]) -> i64 {
        let mut header = serde_json::Map::new();
        header.insert("__metadata__".into(), serde_json::json!({"format": "pt"}));
        let mut at = 0i64;
        for (name, shape, bytes) in tensors {
            header.insert(
                (*name).into(),
                serde_json::json!({"dtype": "BF16", "shape": shape, "data_offsets": [at, at + bytes]}),
            );
            at += bytes;
        }
        let text = serde_json::to_vec(&Value::Object(header)).unwrap();
        let mut data = (text.len() as u64).to_le_bytes().to_vec();
        data.extend_from_slice(&text);
        data.resize(data.len() + at as usize, 0);
        std::fs::write(dir.join(file), &data).unwrap();
        data.len() as i64
    }

    #[test]
    fn layers_shard_their_projections_and_keep_the_rest_whole() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "model-00001-of-00002.safetensors",
            &[
                ("model.embed_tokens.weight", &[8, 4], 64),
                ("model.layers.0.self_attn.q_proj.weight", &[4, 4], 32),
                ("model.layers.0.input_layernorm.weight", &[4], 8),
                ("model.layers.0.mlp.gate.weight", &[2, 4], 16),
                ("model.layers.0.mlp.experts.0.gate_proj.weight", &[4, 4], 32),
                ("model.layers.0.self_attn.o_proj.bias", &[4], 8),
            ],
        );
        write(
            dir.path(),
            "model-00002-of-00002.safetensors",
            &[
                ("model.layers.1.self_attn.q_proj.weight", &[4, 4], 32),
                (
                    "model.layers.1.self_attn.kv_a_proj_with_mqa.weight",
                    &[2, 4],
                    16,
                ),
                ("model.norm.weight", &[4], 8),
                ("lm_head.weight", &[8, 4], 64),
                ("visual.blocks.0.attn.qkv.weight", &[4, 4], 32),
            ],
        );
        let layout = read_checkpoint_layout(dir.path()).unwrap();
        assert_eq!(
            layout,
            CheckpointLayout {
                sharded_bytes: 32 + 32 + 32,
                layer_count: 2,
                largest_layer_bytes: 64,
            }
        );
    }

    // ADR 0014 amendment A20: the n-gram tables an engine option keeps on disk.
    #[test]
    fn tables_are_found_beside_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let table = "language_model.model.layers.0.ple.ple_embedding.ngram_embedding";
        let (shard0, shard1, scale) = (
            format!("{table}.shard_0.weight"),
            format!("{table}.shard_1.weight"),
            format!("{table}.weight_scale"),
        );
        write(
            dir.path(),
            "model-00001-of-00002.safetensors",
            &[
                (
                    "language_model.model.layers.0.self_attn.q_proj.weight",
                    &[4, 4],
                    32,
                ),
                (shard0.as_str(), &[8, 4], 200),
                (shard1.as_str(), &[8, 4], 100),
                (scale.as_str(), &[1], 2),
            ],
        );
        write(
            dir.path(),
            "model-00002-of-00002.safetensors",
            &[
                (
                    "language_model.model.layers.1.self_attn.q_proj.weight",
                    &[4, 4],
                    48,
                ),
                (
                    "ple.ple_embedding.ngram_embedding.shard_0.weight",
                    &[8, 4],
                    10,
                ),
            ],
        );
        let (layout, tables) = read_header_facts(dir.path());
        assert_eq!(
            layout,
            Some(CheckpointLayout {
                sharded_bytes: 32 + 300 + 48,
                layer_count: 2,
                largest_layer_bytes: 332,
            })
        );
        assert_eq!(
            tables,
            Some(CheckpointTables {
                bytes: 310,
                count: 2,
                sharded_bytes: 300,
                resident_largest_layer_bytes: 48,
            })
        );
        // A checkpoint without tables has none.
        let plain = tempfile::tempdir().unwrap();
        write(
            plain.path(),
            "model.safetensors",
            &[("model.layers.0.mlp.up_proj.weight", &[4, 4], 32)],
        );
        assert_eq!(read_checkpoint_tables(plain.path()), None);
        assert!(read_checkpoint_layout(plain.path()).is_some());
    }

    #[test]
    fn a_checkpoint_without_safetensors_has_no_layout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pytorch_model.bin"), b"weights").unwrap();
        assert_eq!(read_checkpoint_layout(dir.path()), None);
        std::fs::write(dir.path().join("broken.safetensors"), b"\xff\xff").unwrap();
        assert_eq!(read_checkpoint_layout(dir.path()), None);
    }

    #[test]
    fn layer_names_are_recognized() {
        assert_eq!(
            sharded_layer("model.layers.12.mlp.down_proj.weight", 2).as_deref(),
            Some("model.layers.12")
        );
        assert_eq!(
            sharded_layer("transformer.h.3.attn.c_attn.weight", 2).as_deref(),
            Some("transformer.h.3")
        );
        assert_eq!(
            sharded_layer("backbone.layers.0.mixer.in_proj.weight", 2).as_deref(),
            Some("backbone.layers.0")
        );
        assert!(sharded_layer("model.layers.0.mlp.gate_proj.weight", 2).is_some());
        assert_eq!(sharded_layer("model.layers.0.mlp.gate.weight", 2), None);
        assert_eq!(
            sharded_layer("model.layers.0.post_attention_layernorm.weight", 1),
            None
        );
        assert_eq!(sharded_layer("model.embed_tokens.weight", 2), None);
        assert_eq!(sharded_layer("ple.ngram_embedding.shard_0", 2), None);
    }
}
