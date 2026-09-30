//! ADR 0014 §3, T14 T21: one reserved table on both sides of the launch.
//!
//! The deploy-time policy (`engine_policy.rs`) refuses reserved option
//! spellings; the protected entries (`runtime/vllm_entry.py`,
//! `runtime/sglang_server_args.py`) refuse reserved parsed destinations. The
//! two lists are read here from source and must name the same settings, so a
//! name added on one side and forgotten on the other fails this test.

use std::collections::BTreeSet;

use capyctl_config::engine_policy::{reserved_families, reserved_options, Engine};

fn runtime_source(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The quoted names inside the first `<start> ... <end>` block of `source`.
fn quoted_block(source: &str, start: &str, end: &str) -> BTreeSet<String> {
    let begin = source.find(start).unwrap_or_else(|| panic!("no `{start}`"));
    let rest = &source[begin + start.len()..];
    let block = &rest[..rest
        .find(end)
        .unwrap_or_else(|| panic!("no end for `{start}`"))];
    let mut names = BTreeSet::new();
    for line in block.lines() {
        let code = line.split('#').next().unwrap_or("");
        let mut parts = code.split('"');
        parts.next();
        while let (Some(name), Some(_)) = (parts.next(), parts.next()) {
            names.insert(name.to_owned());
        }
    }
    names
}

/// Only dictionary keys: `"name": value`.
fn dict_keys(source: &str, start: &str) -> BTreeSet<String> {
    let begin = source.find(start).unwrap();
    let rest = &source[begin + start.len()..];
    let block = &rest[..rest.find("\n}").unwrap()];
    block
        .split('"')
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|pair| pair[1].trim_start().starts_with(':'))
        .map(|pair| pair[0].to_owned())
        .collect()
}

fn dest(option: &str) -> String {
    option.trim_start_matches("--").replace('-', "_")
}

fn covered(families: &[&str], name: &str) -> bool {
    families
        .iter()
        .any(|family| name.starts_with(&dest(family)))
}

// T14 T21
#[test]
fn vllm_reserved_names_match_the_protected_entry() {
    let source = runtime_source("vllm_entry.py");
    let mut python = quoted_block(&source, "RESERVED = (", "\n)");
    python.extend(quoted_block(&source, "RESERVED_IF_PRESENT = (", ")"));
    python.extend(quoted_block(&source, "SLEEP_RESERVED = (", ")"));
    let python_families = quoted_block(&source, "RESERVED_FAMILIES = (", ")");
    let rust: BTreeSet<String> = reserved_options(Engine::Vllm, true)
        .iter()
        .map(|o| dest(o))
        .collect();
    let rust_families = reserved_families(Engine::Vllm);
    // The positional model and the configuration file (refused separately by
    // CONFIG_FILE_OPTIONS) are the entry's alone.
    for name in &python {
        if matches!(name.as_str(), "model_tag" | "config") {
            continue;
        }
        assert!(
            rust.contains(name) || covered(rust_families, name),
            "vllm_entry.py reserves `{name}`, engine_policy.rs does not"
        );
    }
    // Spellings the installed 0.29 parser no longer has are still refused at
    // deploy time; the parser itself refuses them as unknown at launch.
    let retired = [
        "device",
        "swap_space",
        "kv_cache_bytes",
        "kv_cache_memory",
        "disable_log_requests",
    ];
    for name in &rust {
        assert!(
            python.contains(name)
                || python_families.iter().any(|f| name.starts_with(f.as_str()))
                || retired.contains(&name.as_str()),
            "engine_policy.rs reserves `{name}`, vllm_entry.py does not"
        );
    }
    for family in python_families {
        assert!(
            rust_families.iter().any(|f| dest(f) == family),
            "family `{family}` missing from engine_policy.rs"
        );
    }
}

// T14 T21
#[test]
fn sglang_reserved_names_match_the_argument_mapper() {
    let source = runtime_source("sglang_server_args.py");
    let mut python = dict_keys(&source, "_RESERVED_CONSTANT = {");
    python.extend(quoted_block(&source, "_RESERVED_BOUND = (", ")"));
    let python_families = quoted_block(&source, "_RESERVED_FAMILIES = (", ")");
    let rust: BTreeSet<String> = reserved_options(Engine::Sglang, false)
        .iter()
        .map(|o| dest(o))
        .collect();
    let rust_families = reserved_families(Engine::Sglang);
    // Rust also lists the parser's alias spellings (SGLANG_RESERVED_ALIASES),
    // which resolve to fields, so the reverse direction compares fields only.
    for name in &python {
        assert!(
            rust.contains(name) || covered(rust_families, name),
            "sglang_server_args.py reserves `{name}`, engine_policy.rs does not"
        );
    }
    for field in capyctl_config::engine_policy::SGLANG_RESERVED_FIELDS {
        assert!(
            python.contains(*field)
                || python_families
                    .iter()
                    .any(|f| field.starts_with(f.as_str())),
            "engine_policy.rs reserves `{field}`, sglang_server_args.py does not"
        );
    }
    for family in python_families {
        assert!(
            rust_families.iter().any(|f| dest(f) == family),
            "family `{family}` missing from engine_policy.rs"
        );
    }
}
