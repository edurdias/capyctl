//! ADR 0029 §9: the GGUF header reader and the memory request CapyCTL derives
//! for a llama.cpp deployment from it. Synthetic headers written by the
//! tests, no tensor data. CPU tests only; none of this qualifies llama.cpp
//! (only the live rows LC1–LC6 do), and the KV figures still need the live
//! check against llama.cpp's own startup log (design note, live check 3).

use std::io::Read;
use std::path::Path;

use capyctl_config::context_fit::llamacpp::{
    derivation_refusal, kv_cache_bytes, kv_of_header, measure, LlamacppFiles,
};
use capyctl_config::effective::{
    checkpoint_location, decode_effective_snapshot, resolve_effective,
    resolve_effective_with_checkpoint, resolve_snapshot_with_checkpoint, unified_margin,
    CheckpointFacts, EffectiveDeployment, ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES,
    ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES,
};
use capyctl_config::engine_policy::Engine;
use capyctl_config::gguf::{read_header, read_header_file, GgufError, GgufValue};
use capyctl_config::ConfigErrorCode;
use capyctl_domain::gguf::{GgufFacts, GgufKv, GgufKvRefusal, GgufKvShape};
use capyctl_domain::launch::{LaunchSettings, LlamacppGpuLayers};
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;

/// One metadata value of a synthetic header.
enum V {
    U32(u32),
    U64(u64),
    I32(i32),
    F32(f32),
    Bool(bool),
    Str(String),
    U32s(Vec<u32>),
    I32s(Vec<i32>),
    Strs(Vec<String>),
}

fn string(out: &mut Vec<u8>, text: &str) {
    out.extend((text.len() as u64).to_le_bytes());
    out.extend(text.as_bytes());
}

fn value(out: &mut Vec<u8>, value: &V) {
    let scalar = |out: &mut Vec<u8>, kind: u32, bytes: &[u8]| {
        out.extend(kind.to_le_bytes());
        out.extend(bytes);
    };
    let array = |out: &mut Vec<u8>, element: u32, count: usize| {
        out.extend(9u32.to_le_bytes());
        out.extend(element.to_le_bytes());
        out.extend((count as u64).to_le_bytes());
    };
    match value {
        V::U32(v) => scalar(out, 4, &v.to_le_bytes()),
        V::U64(v) => scalar(out, 10, &v.to_le_bytes()),
        V::I32(v) => scalar(out, 5, &v.to_le_bytes()),
        V::F32(v) => scalar(out, 6, &v.to_le_bytes()),
        V::Bool(v) => scalar(out, 7, &[u8::from(*v)]),
        V::Str(text) => {
            out.extend(8u32.to_le_bytes());
            string(out, text);
        }
        V::U32s(values) => {
            array(out, 4, values.len());
            values.iter().for_each(|v| out.extend(v.to_le_bytes()));
        }
        V::I32s(values) => {
            array(out, 5, values.len());
            values.iter().for_each(|v| out.extend(v.to_le_bytes()));
        }
        V::Strs(values) => {
            array(out, 8, values.len());
            values.iter().for_each(|text| string(out, text));
        }
    }
}

/// A GGUF file's header: magic, `version`, a tensor count and `keys`.
fn header(version: u32, keys: &[(&str, V)]) -> Vec<u8> {
    let mut out = b"GGUF".to_vec();
    out.extend(version.to_le_bytes());
    out.extend(291u64.to_le_bytes());
    out.extend((keys.len() as u64).to_le_bytes());
    for (key, v) in keys {
        string(&mut out, key);
        value(&mut out, v);
    }
    out
}

/// A llama-family model: `layers` blocks of 32 heads and `kv_heads` KV
/// heads 128 wide, trained at 32768 tokens.
fn llama(layers: u32, kv_heads: V) -> Vec<(&'static str, V)> {
    vec![
        ("general.architecture", V::Str("llama".into())),
        ("general.name", V::Str("Synthetic".into())),
        ("general.quantization_version", V::U64(2)),
        ("general.file_type", V::I32(15)),
        ("llama.context_length", V::U32(32768)),
        ("llama.embedding_length", V::U32(4096)),
        ("llama.block_count", V::U32(layers)),
        ("llama.attention.head_count", V::U32(32)),
        ("llama.attention.head_count_kv", kv_heads),
        ("llama.attention.key_length", V::U32(128)),
        ("llama.attention.value_length", V::U32(128)),
        ("llama.rope.freq_base", V::F32(500000.0)),
        ("tokenizer.ggml.add_bos_token", V::Bool(true)),
        (
            "tokenizer.ggml.tokens",
            V::Strs((0..5000).map(|i| format!("tok{i}")).collect()),
        ),
        ("tokenizer.ggml.token_type", V::I32s(vec![1; 5000])),
    ]
}

fn shape_of(bytes: &[u8]) -> (Option<u32>, GgufKv) {
    kv_of_header(&read_header(bytes).unwrap())
}

fn attention(layers: u32, k: u64, v: u64, padded: u64) -> GgufKv {
    GgufKv::Attention(GgufKvShape {
        layers,
        k_values: k,
        v_values: v,
        v_values_padded: padded,
    })
}

// T42 (ADR 0029 §9): the reader returns the keys of a llama-family header,
// version 2 or 3, with the per-layer counts kept and the tokenizer skipped,
// and stops at the end of the metadata.
#[test]
fn the_reader_returns_a_llama_header() {
    for version in [2, 3] {
        let mut bytes = header(version, &llama(48, V::U32(4)));
        // Tensor descriptions and data follow; nothing past the metadata is read.
        bytes.extend(vec![0xAB; 4096]);
        let read = read_header(bytes.as_slice()).unwrap();
        assert_eq!(read.version, version);
        assert_eq!(read.tensor_count, 291);
        assert_eq!(read.architecture(), Some("llama"));
        assert_eq!(read.uint("llama.block_count"), Some(48));
        assert_eq!(
            read.per_layer("llama.attention.head_count_kv", 48),
            Some(vec![4; 48])
        );
        assert_eq!(
            read.value("llama.rope.freq_base"),
            Some(&GgufValue::Float(500000.0))
        );
        assert!(read.keys.contains("tokenizer.ggml.tokens"));
        assert_eq!(read.value("tokenizer.ggml.tokens"), None);
        assert_eq!(read.value("general.name"), None);
        assert_eq!(read.uint("general.quantization_version"), Some(2));
        assert_eq!(read.uint("general.file_type"), Some(15));
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.gguf");
    std::fs::write(&path, header(3, &llama(48, V::U32(4)))).unwrap();
    assert_eq!(
        read_header_file(&path).unwrap().architecture(),
        Some("llama")
    );
    assert_eq!(
        read_header_file(&dir.path().join("absent.gguf")),
        Err(GgufError::Io)
    );
}

/// A stream of `prefix` followed by `keys` arrays of `entries` u64 zeros,
/// produced as it is read.
fn huge(prefix: Vec<u8>, keys: usize, entries: u64) -> impl Read {
    let mut stream: Box<dyn Read> = Box::new(std::io::Cursor::new(prefix));
    for index in 0..keys {
        let mut key = Vec::new();
        string(&mut key, &format!("big.{index}"));
        key.extend(9u32.to_le_bytes());
        key.extend(10u32.to_le_bytes());
        key.extend(entries.to_le_bytes());
        stream = Box::new(
            stream
                .chain(std::io::Cursor::new(key))
                .chain(std::io::repeat(0).take(entries * 8)),
        );
    }
    stream
}

// T42 (ADR 0029 §9): a bad magic, a version other than 2 or 3, a truncated
// file and metadata past each bound are refused.
#[test]
fn the_reader_refuses_what_it_cannot_read() {
    let good = header(3, &llama(4, V::U32(4)));
    let mut bad_magic = good.clone();
    bad_magic[..4].copy_from_slice(b"GGML");
    assert_eq!(read_header(bad_magic.as_slice()), Err(GgufError::BadMagic));
    for version in [0, 1, 4] {
        let mut other = good.clone();
        other[4..8].copy_from_slice(&u32::to_le_bytes(version));
        assert_eq!(
            read_header(other.as_slice()),
            Err(GgufError::Version(version))
        );
    }
    for cut in [3, 20, 30, good.len() - 1] {
        assert_eq!(
            read_header(&good[..cut]),
            Err(GgufError::Truncated),
            "cut at {cut}"
        );
    }
    // The key count.
    let mut keys = good.clone();
    keys[16..24].copy_from_slice(&((1u64 << 20) + 1).to_le_bytes());
    assert_eq!(
        read_header(keys.as_slice()),
        Err(GgufError::Bound("key count"))
    );
    // A string longer than 1 MiB.
    let long = header(3, &[("general.name", V::Str("x".repeat((1 << 20) + 1)))]);
    assert_eq!(
        read_header(long.as_slice()),
        Err(GgufError::Bound("string length"))
    );
    // An array of more than 1 048 576 entries.
    let mut prefix = b"GGUF".to_vec();
    prefix.extend(3u32.to_le_bytes());
    prefix.extend(0u64.to_le_bytes());
    prefix.extend(1u64.to_le_bytes());
    assert_eq!(
        read_header(huge(prefix.clone(), 1, (1 << 20) + 1)).err(),
        Some(GgufError::Bound("array length"))
    );
    // More than 64 MiB of metadata: nine arrays of 8 MiB, never held.
    prefix[16..24].copy_from_slice(&9u64.to_le_bytes());
    assert_eq!(
        read_header(huge(prefix, 9, 1 << 20)).err(),
        Some(GgufError::Bound("metadata size"))
    );
    // A duplicate key and an array of arrays are malformed.
    let duplicate = header(3, &[("a", V::U32(1)), ("a", V::U32(2))]);
    assert!(matches!(
        read_header(duplicate.as_slice()),
        Err(GgufError::Malformed(_))
    ));
    let mut nested = header(3, &[]);
    nested[16..24].copy_from_slice(&1u64.to_le_bytes());
    string(&mut nested, "nested");
    nested.extend(9u32.to_le_bytes());
    nested.extend(9u32.to_le_bytes());
    nested.extend(1u64.to_le_bytes());
    assert!(matches!(
        read_header(nested.as_slice()),
        Err(GgufError::Malformed(_))
    ));
}

// T42 (ADR 0029 §9, plan L4): the KV of a 48-layer, 4-KV-head, 128-wide
// model at 32768 tokens and 4 slots in f16 is 4 × 32768 × 48 × 4 × (128 × 2
// + 128 × 2) bytes; q8_0 takes 34 bytes per 32 values; a per-layer
// head_count_kv is summed; the widths fall back to embedding_length /
// head_count; MTP layers are not the main context's.
#[test]
fn the_kv_cache_follows_the_formula() {
    let (trained, kv) = shape_of(&header(3, &llama(48, V::U32(4))));
    assert_eq!(trained, Some(32768));
    let values = 48 * 4 * 128;
    assert_eq!(kv, attention(48, values, values, values));
    let GgufKv::Attention(shape) = kv else {
        unreachable!()
    };
    assert_eq!(
        kv_cache_bytes(&shape, 32768, 4, "f16", false),
        Some(4 * 32768 * 48 * 4 * (128 * 2 + 128 * 2))
    );
    assert_eq!(
        kv_cache_bytes(&shape, 32768, 4, "q8_0", false),
        Some(4 * 32768 * 2 * (48 * 4 * 128 / 32 * 34))
    );
    assert_eq!(
        kv_cache_bytes(&shape, 32768, 4, "f32", false),
        Some(4 * 32768 * 48 * 4 * (128 * 4 + 128 * 4))
    );
    // Each slot's window is padded to 256 cells: 15000 tokens hold 15104.
    assert_eq!(
        kv_cache_bytes(&shape, 15000, 4, "f16", false),
        Some(4 * 15104 * 48 * 4 * 512)
    );
    assert_eq!(kv_cache_bytes(&shape, 32768, 4, "fp8", false), None);

    // One count per layer, summed: 24 layers of 8 KV heads, 24 of 2.
    let per_layer: Vec<u32> = (0..48)
        .map(|layer| if layer < 24 { 8 } else { 2 })
        .collect();
    let (_, kv) = shape_of(&header(3, &llama(48, V::U32s(per_layer))));
    let summed = (24 * 8 + 24 * 2) * 128;
    // Without flash attention llama.cpp pads every V row to the widest.
    assert_eq!(kv, attention(48, summed, summed, 48 * 8 * 128));
    let GgufKv::Attention(shape) = kv else {
        unreachable!()
    };
    assert_eq!(
        kv_cache_bytes(&shape, 8192, 1, "f16", true),
        Some(8192 * (summed as i64) * 2 * 2)
    );
    assert_eq!(
        kv_cache_bytes(&shape, 8192, 1, "f16", false),
        Some(8192 * ((summed as i64) * 2 + 48 * 8 * 128 * 2))
    );
    // An array of another length is unreadable.
    let (_, kv) = shape_of(&header(3, &llama(48, V::U32s(vec![8; 47]))));
    assert_eq!(kv, GgufKv::Refused(GgufKvRefusal::Unreadable));

    // No key or value length: embedding_length / head_count (4096 / 32).
    let fallback: Vec<_> = llama(32, V::U32(8))
        .into_iter()
        .filter(|(key, _)| {
            !matches!(
                *key,
                "llama.attention.key_length" | "llama.attention.value_length"
            )
        })
        .collect();
    let (_, kv) = shape_of(&header(3, &fallback));
    assert_eq!(kv, attention(32, 32 * 8 * 128, 32 * 8 * 128, 32 * 8 * 128));
    // KV heads default to the heads.
    let no_kv: Vec<_> = llama(2, V::U32(0))
        .into_iter()
        .filter(|(key, _)| *key != "llama.attention.head_count_kv")
        .collect();
    let (_, kv) = shape_of(&header(3, &no_kv));
    assert_eq!(kv, attention(2, 2 * 32 * 128, 2 * 32 * 128, 2 * 32 * 128));
    // The MTP layer of a 49-block model keeps its cache in the MTP context.
    let mut mtp = llama(49, V::U32(4));
    mtp.push(("llama.nextn_predict_layers", V::U32(1)));
    let (_, kv) = shape_of(&header(3, &mtp));
    assert_eq!(kv, attention(48, values, values, values));
}

// T42 (ADR 0029 §9): sliding-window, recurrent (`ssm.*`), hybrid
// (`full_attention_interval`), MLA (`kv_lora_rank`) and other cache layouts
// are named, never derived.
#[test]
fn headers_with_another_cache_layout_are_refused() {
    use GgufKvRefusal::*;
    let with = |arch: &str, extra: Vec<(&'static str, V)>| {
        let mut keys: Vec<(String, V)> = llama(4, V::U32(4))
            .into_iter()
            .map(|(key, v)| (key.replace("llama.", &format!("{arch}.")), v))
            .collect();
        keys[0].1 = V::Str(arch.into());
        keys.extend(
            extra
                .into_iter()
                .map(|(key, v)| (key.replace("llama.", &format!("{arch}.")), v)),
        );
        let keys: Vec<(&str, V)> = keys.iter().map(|(k, v)| (k.as_str(), clone(v))).collect();
        shape_of(&header(3, &keys)).1
    };
    for (arch, extra, refusal) in [
        (
            "llama",
            vec![("llama.attention.sliding_window", V::U32(4096))],
            SlidingWindow,
        ),
        (
            "llama",
            vec![("llama.attention.sliding_window_pattern", V::U32(4))],
            SlidingWindow,
        ),
        ("gemma2", vec![], SlidingWindow),
        ("gpt-oss", vec![], SlidingWindow),
        ("llama4", vec![], SlidingWindow),
        (
            "llama",
            vec![("llama.ssm.state_size", V::U32(16))],
            Recurrent,
        ),
        ("mamba2", vec![], Recurrent),
        ("rwkv7", vec![], Recurrent),
        (
            "llama",
            vec![("llama.full_attention_interval", V::U32(4))],
            Hybrid,
        ),
        ("qwen35", vec![], Hybrid),
        ("nemotron_h", vec![], Hybrid),
        (
            "deepseek2",
            vec![("llama.attention.kv_lora_rank", V::U32(512))],
            Mla,
        ),
        ("bert", vec![], Layout),
        ("clip", vec![], Layout),
        (
            "llama",
            vec![("llama.attention.indexer.head_count", V::U32(4))],
            Layout,
        ),
    ] {
        assert_eq!(
            with(arch, extra),
            GgufKv::Refused(refusal),
            "{arch} {refusal:?}"
        );
    }
    // A window of 0 is no window: llama4 then has full attention.
    assert!(matches!(
        with(
            "llama4",
            vec![("llama.attention.sliding_window", V::U32(0))]
        ),
        GgufKv::Attention(_)
    ));
    assert!(matches!(
        with("llama", vec![("llama.attention.sliding_window", V::U32(0))]),
        GgufKv::Attention(_)
    ));
    // No architecture, or no layers: nothing to derive from.
    let no_arch: Vec<_> = llama(4, V::U32(4)).into_iter().skip(1).collect();
    assert_eq!(
        shape_of(&header(3, &no_arch)),
        (None, GgufKv::Refused(Unreadable))
    );
    let no_blocks: Vec<_> = llama(4, V::U32(4))
        .into_iter()
        .filter(|(key, _)| *key != "llama.block_count")
        .collect();
    assert_eq!(
        shape_of(&header(3, &no_blocks)).1,
        GgufKv::Refused(Unreadable)
    );
}

fn clone(value: &V) -> V {
    match value {
        V::U32(v) => V::U32(*v),
        V::U64(v) => V::U64(*v),
        V::I32(v) => V::I32(*v),
        V::F32(v) => V::F32(*v),
        V::Bool(v) => V::Bool(*v),
        V::Str(v) => V::Str(v.clone()),
        V::U32s(v) => V::U32s(v.clone()),
        V::I32s(v) => V::I32s(v.clone()),
        V::Strs(v) => V::Strs(v.clone()),
    }
}

fn write(path: &Path, bytes: &[u8], total: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    use std::io::Write;
    (&file).write_all(bytes).unwrap();
    file.set_len(total).unwrap();
}

// T42 (ADR 0029 §8, §9, ADR 0014 amendment A6): the weights a launch loads
// are the rendered GGUF and all its shards, the projector and a draft model in
// an approved path with all its shards, not every GGUF in the checkpoint; the
// header is the first shard's. A draft that cannot be counted (outside the
// approved paths, a shard missing, a set named by a later shard) is refused,
// never dropped; several models without `gguf_file`, or an unreadable header,
// are named.
#[test]
fn the_measured_weights_are_what_the_launch_loads() {
    let root = tempfile::tempdir().unwrap();
    let checkpoint = root.path().join("model");
    let first = header(3, &llama(48, V::U32(4)));
    // Tensor data follows the header; the file is sized past it.
    write(
        &checkpoint.join("Q4/m-00001-of-00002.gguf"),
        &first,
        400_000,
    );
    write(&checkpoint.join("Q4/m-00002-of-00002.gguf"), b"", 3000);
    write(&checkpoint.join("mmproj-F16.gguf"), b"GGUF", 500);
    let drafts = root.path().join("drafts");
    write(&drafts.join("d.gguf"), b"GGUF", 70);
    write(&root.path().join("elsewhere/d.gguf"), b"GGUF", 9);
    let files = |gguf: Option<&str>, mmproj: Option<&str>, draft: &Path| {
        LlamacppFiles::new(
            gguf.map(str::to_owned),
            mmproj.map(str::to_owned),
            &["--model-draft".to_owned(), draft.display().to_string()],
            &[drafts.display().to_string()],
        )
    };
    let bare = |gguf: Option<&str>, mmproj: Option<&str>| {
        LlamacppFiles::new(gguf.map(str::to_owned), mmproj.map(str::to_owned), &[], &[])
    };
    let facts = measure(
        &checkpoint,
        &files(None, Some("mmproj-F16.gguf"), &drafts.join("d.gguf")),
    );
    let values = 48 * 4 * 128;
    assert_eq!(
        facts,
        GgufFacts {
            weights_bytes: 400_000 + 3000 + 500 + 70,
            training_context: Some(32768),
            kv: attention(48, values, values, values),
        }
    );
    let outside = measure(
        &checkpoint,
        &files(None, None, &root.path().join("elsewhere/d.gguf")),
    );
    assert_eq!(
        outside,
        GgufFacts {
            weights_bytes: 0,
            training_context: None,
            kv: GgufKv::Refused(GgufKvRefusal::Draft),
        }
    );
    // A split draft counts every shard llama.cpp loads (found by review: only
    // the first shard was counted).
    write(&drafts.join("s-00001-of-00003.gguf"), b"GGUF", 70);
    write(&drafts.join("s-00002-of-00003.gguf"), b"", 700);
    write(&drafts.join("s-00003-of-00003.gguf"), b"", 7000);
    let split = measure(
        &checkpoint,
        &files(None, None, &drafts.join("s-00001-of-00003.gguf")),
    );
    assert_eq!(split.weights_bytes, 403_000 + 70 + 700 + 7000);
    // A set named by a later shard, a missing shard, and a shard that leaves
    // the approved paths through a symlink are refused.
    let later = measure(
        &checkpoint,
        &files(None, None, &drafts.join("s-00002-of-00003.gguf")),
    );
    assert_eq!(later.kv, GgufKv::Refused(GgufKvRefusal::Draft));
    std::fs::remove_file(drafts.join("s-00003-of-00003.gguf")).unwrap();
    let incomplete = measure(
        &checkpoint,
        &files(None, None, &drafts.join("s-00001-of-00003.gguf")),
    );
    assert_eq!(incomplete.kv, GgufKv::Refused(GgufKvRefusal::Draft));
    std::os::unix::fs::symlink(
        root.path().join("elsewhere/d.gguf"),
        drafts.join("s-00003-of-00003.gguf"),
    )
    .unwrap();
    let escaped = measure(
        &checkpoint,
        &files(None, None, &drafts.join("s-00001-of-00003.gguf")),
    );
    assert_eq!(escaped.kv, GgufKv::Refused(GgufKvRefusal::Draft));
    // A second quantization: one must be named.
    write(&checkpoint.join("m-Q8_0.gguf"), &first, 800_000);
    let several = measure(&checkpoint, &bare(None, None));
    assert_eq!(several.kv, GgufKv::Refused(GgufKvRefusal::NoModel));
    assert_eq!(several.weights_bytes, 0);
    let named = measure(&checkpoint, &bare(Some("m-Q8_0.gguf"), None));
    assert_eq!(named.weights_bytes, 800_000);
    // A projector that is not there.
    let missing = measure(&checkpoint, &bare(Some("m-Q8_0.gguf"), Some("absent.gguf")));
    assert_eq!(missing.kv, GgufKv::Refused(GgufKvRefusal::NoModel));
    // A model whose header is not GGUF.
    write(&checkpoint.join("broken.gguf"), b"not a header", 100);
    let broken = measure(&checkpoint, &bare(Some("broken.gguf"), None));
    assert_eq!(
        broken,
        GgufFacts {
            weights_bytes: 100,
            training_context: None,
            kv: GgufKv::Refused(GgufKvRefusal::Unreadable),
        }
    );
}

/// The lab host with a llama.cpp profile, and a deployment that states no
/// memory: 8192 tokens in 4 slots on the unified pool.
fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "llamacpp".into();
    profile["executable"] = "/opt/llama.cpp/bin/llama-server".into();
    profile["build_fingerprint"] = "0.6.0+d812350".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    profile["security"]["approved_paths"] = json!(["/srv/drafts"]);
    profile["security"]["approved_options"] = json!(["--model-draft"]);
    deployment["residency"] = "restart_only".into();
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

/// A discrete host: host RAM in `system`, the card in `gpu0` (14848 MiB).
fn discrete(host: &mut Value) {
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
}

/// 32 layers of 8 KV heads 128 wide (128 KiB per token in f16), trained at
/// 32768 tokens, `weights` loaded, beside a checkpoint of `whole` bytes.
fn facts(whole: i64, weights: i64, kv: GgufKv) -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(whole),
        gguf: Some(GgufFacts {
            weights_bytes: weights,
            training_context: Some(32768),
            kv,
        }),
        ..CheckpointFacts::default()
    }
}

fn dense() -> GgufKv {
    let values = 32 * 8 * 128;
    attention(32, values, values, values)
}

/// 4 slots × 8192 tokens × 128 KiB.
const KV: i64 = 4 * GIB;

fn memory(effective: &EffectiveDeployment) -> &capyctl_domain::launch::MemoryRequest {
    match &effective.engine_config {
        LaunchSettings::Llamacpp(settings) => &settings.memory,
        other => panic!("llama.cpp settings, not {other:?}"),
    }
}

fn phase_bytes(effective: &EffectiveDeployment) -> [Vec<i64>; 5] {
    let r = &effective.resources;
    [&r.cold, &r.ready, &r.parking, &r.parked, &r.wake]
        .map(|phase| phase.allocations.iter().map(|a| a.bytes).collect())
}

// T42 T26 (ADR 0029 §9, ADR 0014 §5, A18, ADR 0019 §3): the derived request
// is the loaded weights + the KV + the margin. Unified memory: the margin
// grows with the weights; a discrete GPU: weights x 1.10 + KV on the card and
// the engine's host RAM beside it. Cold, Ready, parking and wake are the
// request, parked nothing. The whole checkpoint (a second quantization
// beside the rendered one) is what a plan names; the snapshot re-derives.
#[test]
fn the_derived_request_is_weights_kv_and_margin() {
    let (deployment, mut host) = fixture();
    let (whole, weights) = (12 * GIB, 5 * GIB);
    let effective =
        resolve_effective_with_checkpoint(&deployment, &host, facts(whole, weights, dense()))
            .unwrap();
    let memory = memory(&effective);
    let margin = unified_margin(Engine::Llamacpp, Some(weights));
    assert_eq!(memory.kv_cache_bytes, KV);
    assert_eq!(memory.weights_bytes, Some(weights));
    assert_eq!(memory.request_bytes, weights + KV + margin);
    assert_eq!(memory.startup_bytes, Some(memory.request_bytes));
    assert_eq!(memory.checkpoint_weights_bytes(), Some(whole));
    let charged = memory.request_bytes + ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
    assert_eq!(
        phase_bytes(&effective),
        [
            vec![charged],
            vec![charged],
            vec![charged],
            vec![0],
            vec![charged]
        ]
    );
    let text = serde_json::to_string(&effective).unwrap();
    assert!(
        text.contains("\"gguf\":{\"checkpoint_weights_bytes\""),
        "{text}"
    );
    assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);
    // A re-measurement re-derives the request from the new facts.
    let larger = resolve_snapshot_with_checkpoint(&text, facts(whole, 6 * GIB, dense())).unwrap();
    assert_eq!(
        memory_of(&larger),
        6 * GIB + KV + unified_margin(Engine::Llamacpp, Some(6 * GIB))
    );

    discrete(&mut host);
    let effective =
        resolve_effective_with_checkpoint(&deployment, &host, facts(whole, weights, dense()))
            .unwrap();
    let request = weights / 100 * 110 + KV;
    assert_eq!(memory_of(&effective), request);
    let card = request + ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
    let ram = ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES;
    assert_eq!(
        phase_bytes(&effective),
        [
            vec![card, ram],
            vec![card, ram],
            vec![card, ram],
            vec![0, 0],
            vec![card, ram]
        ]
    );
    let text = serde_json::to_string(&effective).unwrap();
    assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);
}

fn memory_of(effective: &EffectiveDeployment) -> i64 {
    memory(effective).request_bytes
}

// T42 (ADR 0029 §9, ADR 0014 §7): unmeasured, the deployment is accepted
// provisional with a pending header and re-resolved once a host measured it.
#[test]
fn an_unmeasured_deployment_is_provisional() {
    let (deployment, host) = fixture();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::NotMaterializable);
    assert!(error.path.starts_with("engine_config.memory"), "{error}");
    let provisional =
        resolve_effective_with_checkpoint(&deployment, &host, CheckpointFacts::provisional())
            .unwrap();
    let text = serde_json::to_string(&provisional).unwrap();
    assert_eq!(decode_effective_snapshot(&text).unwrap(), provisional);
    let measured =
        resolve_snapshot_with_checkpoint(&text, facts(7 * GIB, 5 * GIB, dense())).unwrap();
    assert_eq!(memory(&measured).kv_cache_bytes, KV);
    assert_eq!(memory(&measured).checkpoint_weights_bytes(), Some(7 * GIB));
    // Facts from a host that read no header are not derived from.
    let error = resolve_effective_with_checkpoint(
        &deployment,
        &host,
        CheckpointFacts {
            weights_bytes: Some(GIB),
            ..CheckpointFacts::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.path, "engine_config.memory.kv_cache");
}

fn refused(error: &capyctl_config::ConfigError) {
    assert_eq!(error.code, ConfigErrorCode::MissingRequired, "{error}");
    assert_eq!(error.path, "engine_config.memory.kv_cache", "{error}");
    assert!(
        error.detail.contains("engine_config.memory.kv_cache")
            && error.detail.contains("resources"),
        "{error}"
    );
}

// T42 (ADR 0029 §9): a sliding-window, recurrent, hybrid or MLA header,
// `--spec-type draft-mtp` (or a draft model), `n_gpu_layers: 20`,
// `--override-tensor`, `--cpu-moe`, `--n-cpu-moe`, `--n-cpu-ffn`,
// `--no-kv-offload` and an `mlock` load mode each refuse derivation and name
// `memory.kv_cache` or `resources`; a declared KV cache or a declared
// `resources` short form then resolves, the latter unchanged.
#[test]
fn derivation_is_refused_where_the_cache_is_not_derivable() {
    use GgufKvRefusal::*;
    let (deployment, host) = fixture();
    for refusal in [SlidingWindow, Recurrent, Hybrid, Mla, Layout, Unreadable] {
        let error = resolve_effective_with_checkpoint(
            &deployment,
            &host,
            facts(8 * GIB, 5 * GIB, GgufKv::Refused(refusal)),
        )
        .unwrap_err();
        refused(&error);
        assert!(error.detail.contains(refusal.reason()), "{error}");
    }
    // Weights the host could not measure size nothing, declared or not.
    let mut declared = deployment.clone();
    declared["engine_config"]["memory"] = json!({"kv_cache": "6GiB"});
    for refusal in [NoModel, Draft] {
        for deployment in [&deployment, &declared] {
            let error = resolve_effective_with_checkpoint(
                deployment,
                &host,
                facts(8 * GIB, 0, GgufKv::Refused(refusal)),
            )
            .unwrap_err();
            assert_eq!(error.path, "engine_config.memory", "{error}");
            assert!(error.detail.contains(refusal.reason()), "{error}");
        }
    }
    let with = |engine_config: Value| {
        let mut deployment = deployment.clone();
        deployment["engine_config"] = engine_config;
        resolve_effective_with_checkpoint(&deployment, &host, facts(8 * GIB, 5 * GIB, dense()))
    };
    let extra = |args: Value| {
        with(json!({"context_length": 8192, "accept_extra_args": true, "extra_args": args}))
    };
    for args in [
        json!(["--spec-type", "ngram-mod,draft-mtp"]),
        json!(["--spec-type", "draft-simple"]),
        json!(["--model-draft", "/srv/drafts/d.gguf"]),
        json!(["--override-tensor", "exps=CPU"]),
        json!(["--cpu-moe"]),
        json!(["--n-cpu-moe", "4"]),
        json!(["--n_cpu_ffn", "4"]),
        json!(["--no-kv-offload"]),
        json!(["--load-mode", "mlock"]),
        json!(["--load-mode", "mmap+mlock"]),
    ] {
        refused(&extra(args.clone()).unwrap_err());
    }
    refused(&with(json!({"context_length": 8192, "llamacpp": {"n_gpu_layers": 20}})).unwrap_err());
    // Weight-free speculation, the default load mode and every layer derive.
    for args in [
        json!(["--spec-type", "ngram-mod"]),
        json!(["--load-mode", "mmap"]),
        json!(["--cache-reuse", "256"]),
    ] {
        assert_eq!(memory(&extra(args).unwrap()).kv_cache_bytes, KV);
    }
    assert_eq!(
        memory(
            &with(json!({"context_length": 8192, "llamacpp": {"n_gpu_layers": "all"}})).unwrap()
        )
        .kv_cache_bytes,
        KV
    );
    // A declared KV cache sizes the request with the loaded weights,
    // the draft model's included.
    let declared = with(
        json!({"context_length": 8192, "memory": {"kv_cache": "6GiB"},
        "accept_extra_args": true, "extra_args": ["--model-draft", "/srv/drafts/d.gguf"]}),
    )
    .unwrap();
    assert_eq!(memory(&declared).kv_cache_bytes, 6 * GIB);
    assert_eq!(
        memory(&declared).request_bytes,
        5 * GIB + 6 * GIB + unified_margin(Engine::Llamacpp, Some(5 * GIB))
    );
    // A declared short form resolves unchanged, whatever the header.
    let mut short = deployment.clone();
    short["resources"] = json!({"gpu": "11GiB", "ram": "2GiB"});
    let effective = resolve_effective_with_checkpoint(
        &short,
        &host,
        facts(8 * GIB, 5 * GIB, GgufKv::Refused(SlidingWindow)),
    )
    .unwrap();
    assert_eq!(phase_bytes(&effective)[1], vec![13 * GIB]);
    assert_eq!(memory(&effective).kv_cache_bytes, 13 * GIB - 5 * GIB);
}

// T42 T26 (ADR 0029 §5, §9, SPEC §3; found by review): with `--fit off`
// llama-server allocates the whole cache its context and slots fix, so where
// the header makes that cache calculable a declared KV cache, request or
// `resources` that leaves less for it is refused naming the field; one that
// holds it resolves as declared. A draft context only adds to the cache, so
// the bound stands beside a draft model. Where CapyCTL cannot tell (another
// cache layout, cache layers off the GPU) the declared estimate stands.
#[test]
fn a_declared_budget_holds_the_calculable_cache() {
    let (deployment, host) = fixture();
    let margin = unified_margin(Engine::Llamacpp, Some(5 * GIB));
    let with = |engine_config: Value, resources: Option<Value>, kv: GgufKv| {
        let mut deployment = deployment.clone();
        deployment["engine_config"] = engine_config;
        if let Some(resources) = resources {
            deployment["resources"] = resources;
        }
        resolve_effective_with_checkpoint(&deployment, &host, facts(8 * GIB, 5 * GIB, kv))
    };
    let memory_only = |memory: Value| json!({"context_length": 8192, "memory": memory});
    let below = [
        (
            memory_only(json!({"kv_cache": "1GiB"})),
            None,
            "engine_config.memory.kv_cache",
        ),
        (
            memory_only(json!({"request": format!("{}B", 5 * GIB + KV + margin - 1)})),
            None,
            "engine_config.memory.request",
        ),
        (
            json!({"context_length": 8192}),
            Some(json!({"gpu": "7GiB", "ram": "1GiB"})),
            "resources",
        ),
        (
            json!({"context_length": 8192, "memory": {"kv_cache": "1GiB"},
                   "accept_extra_args": true, "extra_args": ["--model-draft", "/srv/drafts/d.gguf"]}),
            None,
            "engine_config.memory.kv_cache",
        ),
    ];
    for (engine_config, resources, path) in below {
        let error = with(engine_config, resources, dense()).unwrap_err();
        assert_eq!(error.path, path, "{error}");
        assert!(error.detail.contains(&KV.to_string()), "{error}");
    }
    let kv = memory(&with(memory_only(json!({"kv_cache": "4GiB"})), None, dense()).unwrap())
        .kv_cache_bytes;
    assert_eq!(kv, KV);
    let request = format!("{}B", 5 * GIB + KV + margin);
    let effective = with(memory_only(json!({"request": request})), None, dense()).unwrap();
    assert_eq!(memory(&effective).kv_cache_bytes, KV);
    let effective = with(
        json!({"context_length": 8192}),
        Some(json!({"gpu": "9GiB", "ram": "1GiB"})),
        dense(),
    )
    .unwrap();
    assert_eq!(memory(&effective).kv_cache_bytes, 10 * GIB - 5 * GIB);
    // Unknowable: another layout, or cache layers off the GPU.
    let small = json!({"kv_cache": "1GiB"});
    for (engine_config, kv) in [
        (
            memory_only(small.clone()),
            GgufKv::Refused(GgufKvRefusal::SlidingWindow),
        ),
        (
            json!({"context_length": 8192, "memory": small,
                   "llamacpp": {"n_gpu_layers": 20}}),
            dense(),
        ),
        (
            json!({"context_length": 8192, "memory": small,
                   "accept_extra_args": true, "extra_args": ["--no-kv-offload"]}),
            dense(),
        ),
    ] {
        let effective = with(engine_config, None, kv).unwrap();
        assert_eq!(memory(&effective).kv_cache_bytes, GIB);
    }
}

// T42 T26 (ADR 0029 §8, §9, ADR 0014 amendment A6; found by review): the draft
// a host-fixed or an extra `--model-draft` names is measured with every shard
// where a host measures a checkpoint (located without resolving) and where it
// launches one (resolved) alike. A host-fixed draft outside
// security.approved_paths is refused at resolution, declared budget or not,
// instead of being left out of it.
#[test]
fn a_named_draft_is_counted_whole_or_refused() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    write(
        &store.join("model/m.gguf"),
        &header(3, &llama(32, V::U32(8))),
        1000,
    );
    let drafts = root.path().join("drafts");
    write(&drafts.join("d-00001-of-00002.gguf"), b"GGUF", 70);
    write(&drafts.join("d-00002-of-00002.gguf"), b"", 700);
    write(&root.path().join("elsewhere/d.gguf"), b"GGUF", 9);
    let (mut deployment, mut host) = fixture();
    host["model_store"]["path"] = json!(store);
    host["runtime_profiles"]["local"]["security"]["approved_paths"] = json!([drafts]);
    deployment["model"]["path"] = json!(store.join("model"));
    deployment["engine_config"] = json!({"context_length": 8192, "memory": {"kv_cache": "4GiB"}});
    let first = drafts.join("d-00001-of-00002.gguf").display().to_string();
    let resolve = |deployment: &Value, host: &Value| {
        let gguf = checkpoint_location(deployment, host).unwrap().gguf_facts();
        let facts = CheckpointFacts {
            weights_bytes: Some(1000),
            gguf,
            ..CheckpointFacts::default()
        };
        (
            gguf,
            resolve_effective_with_checkpoint(deployment, host, facts),
        )
    };
    // An extra draft and a host-fixed one inside the approved paths.
    let mut extra = deployment.clone();
    extra["engine_config"]["accept_extra_args"] = json!(true);
    extra["engine_config"]["extra_args"] = json!(["--model-draft", first]);
    let mut fixed = host.clone();
    fixed["runtime_profiles"]["local"]["args"] = json!(["--model-draft", first]);
    for (deployment, host) in [(&extra, &host), (&deployment, &fixed)] {
        let (gguf, effective) = resolve(deployment, host);
        assert_eq!(gguf.unwrap().weights_bytes, 1000 + 70 + 700);
        let effective = effective.unwrap();
        assert_eq!(memory(&effective).weights_bytes, Some(1000 + 70 + 700));
        assert_eq!(effective.gguf_facts(), gguf, "the launch measures the same");
    }
    // A host-fixed draft outside the approved paths.
    let outside = root.path().join("elsewhere/d.gguf").display().to_string();
    fixed["runtime_profiles"]["local"]["args"] = json!(["--model-draft", outside]);
    let (gguf, effective) = resolve(&deployment, &fixed);
    assert_eq!(gguf.unwrap().kv, GgufKv::Refused(GgufKvRefusal::Draft));
    let error = effective.unwrap_err();
    assert_eq!(error.path, "engine_config.memory", "{error}");
    assert!(
        error.detail.contains(GgufKvRefusal::Draft.reason()),
        "{error}"
    );
}

// T42 (ADR 0029 §5, review focus 5): a model trained at 32k deployed at
// 65536 is refused at resolution, declared resources or not: llama.cpp would
// cap the slot and still allocate the larger cache.
#[test]
fn a_context_above_the_training_context_is_refused() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({"context_length": 65536, "max_concurrent_requests": 1});
    let error =
        resolve_effective_with_checkpoint(&deployment, &host, facts(8 * GIB, 5 * GIB, dense()))
            .unwrap_err();
    assert_eq!(error.path, "engine_config.context_length");
    assert!(error.detail.contains("32768"), "{error}");
    deployment["resources"] = json!({"gpu": "30GiB", "ram": "1GiB"});
    let error = resolve_effective_with_checkpoint(
        &deployment,
        &host,
        facts(
            8 * GIB,
            5 * GIB,
            GgufKv::Refused(GgufKvRefusal::SlidingWindow),
        ),
    )
    .unwrap_err();
    assert_eq!(error.path, "engine_config.context_length");
    // The training context itself is accepted.
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["engine_config"] = json!({"context_length": 32768, "max_concurrent_requests": 1});
    let effective =
        resolve_effective_with_checkpoint(&deployment, &host, facts(8 * GIB, 5 * GIB, dense()))
            .unwrap();
    assert_eq!(memory(&effective).kv_cache_bytes, KV);
}

// T42 (ADR 0029 §9): the files a host measures are the ones the deployment
// names, located without resolving it, and the arguments' refusals stand on
// their own.
#[test]
fn the_host_measures_the_files_the_deployment_names() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({"context_length": 8192,
        "llamacpp": {"gguf_file": "Q8_0/m.gguf", "mmproj_file": "mmproj.gguf"},
        "accept_extra_args": true, "extra_args": ["--model_draft", "/srv/drafts/d.gguf"]});
    let location = checkpoint_location(&deployment, &host).unwrap();
    assert_eq!(
        location.llamacpp,
        Some(LlamacppFiles {
            gguf_file: Some("Q8_0/m.gguf".into()),
            mmproj_file: Some("mmproj.gguf".into()),
            draft: Some("/srv/drafts/d.gguf".into()),
            approved_paths: vec!["/srv/drafts".into()],
        })
    );
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let vllm = checkpoint_location(&all["deployment"], &all["host"]).unwrap();
    assert_eq!(vllm.llamacpp, None);
    assert_eq!(vllm.gguf_facts(), None);
    assert_eq!(derivation_refusal(LlamacppGpuLayers::All, &[]), None);
    assert!(derivation_refusal(LlamacppGpuLayers::Count(99), &[])
        .unwrap()
        .contains("n_gpu_layers 99"));
}
