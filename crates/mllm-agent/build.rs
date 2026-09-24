//! SPEC §3.3 / ADR 0001 (owner decision 2026-09-24): the release is one
//! self-contained executable. mllm's Python runtime helpers (`runtime/*.py`,
//! never `runtime/tests`) are compiled into it with a manifest of their
//! SHA-256 digests; `embedded_runtime` materializes them at role start.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Files the entries import on every engine family; a runtime tree without
/// them is not one this binary may ship (`runtime_integrity::required_files`).
const REQUIRED: &[&str] = &[
    "mllm_vllm_guard.py",
    "vllm_entry.py",
    "engine_capabilities.py",
    "sglang_entry.py",
    "pinned_file_observation.py",
    "sglang_device.py",
];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let runtime = manifest_dir
        .join("..")
        .join("..")
        .join("runtime")
        .canonicalize()
        .expect("the workspace runtime/ directory");
    // A directory: cargo rescans it for any change.
    println!("cargo:rerun-if-changed={}", runtime.display());

    let mut names = Vec::new();
    for entry in std::fs::read_dir(&runtime).expect("runtime/ is readable") {
        let entry = entry.expect("a runtime/ entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("py") {
            continue;
        }
        let file_type = std::fs::symlink_metadata(&path)
            .expect("metadata")
            .file_type();
        assert!(
            file_type.is_file(),
            "runtime/{} is not a regular file; the embedded runtime ships no links",
            entry.file_name().to_string_lossy()
        );
        names.push(
            entry
                .file_name()
                .into_string()
                .expect("a UTF-8 module name"),
        );
    }
    names.sort();
    for required in REQUIRED {
        assert!(
            names.iter().any(|name| name == required),
            "runtime/{required} is missing; the embedded runtime would not launch an engine"
        );
    }

    let mut source = String::from("pub(crate) static FILES: &[EmbeddedFile] = &[\n");
    let mut manifest = Sha256::new();
    for name in &names {
        let path = runtime.join(name);
        let bytes = std::fs::read(&path).expect("a runtime module");
        let digest = hex(&Sha256::digest(&bytes));
        manifest.update(format!("{digest}  {name}\n").as_bytes());
        writeln!(
            source,
            "    EmbeddedFile {{ name: {name:?}, sha256: {digest:?}, contents: include_bytes!({:?}) }},",
            path.display().to_string()
        )
        .expect("write to a string");
    }
    source.push_str("];\n");
    writeln!(
        source,
        "pub(crate) const MANIFEST_DIGEST: &str = {:?};",
        hex(&manifest.finalize())
    )
    .expect("write to a string");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it"));
    std::fs::write(out.join("embedded_runtime.rs"), source).expect("the generated manifest");
}
