//! ADR 0039: TensorFold's native 1.x line, the Zig binary registered from its
//! release directory (`bin/tensorfold-native`) beside the 0.6 Python line.
//!
//! The line is a property of the installation, fixed at registration from its
//! form, never from its version number (§1). This module holds what both the
//! server's resolution and the host's launch need to agree on: the exact-name
//! option tables (§4), the zero-retention refusals (§4), the closed
//! environment's names (§5), the certified versions and checkpoint (§3, §7)
//! and what `engine add` records from the capabilities document (§2).
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::engine_policy::{parse_options, ExtraArgsContext, ProfileArgError};

/// ADR 0039 §1: the two lines of the engine kind `tensorfold`. A profile
/// without `line` is the Python line, so every existing `engines.yaml` keeps
/// its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TensorfoldLine {
    /// TensorFold 0.6.x, a Python environment with a `tensorfold-*.dist-info`
    /// (ADR 0023 as it stands).
    #[default]
    Python,
    /// TensorFold 1.x, a release root holding `bin/tensorfold-native`.
    Native,
}

impl TensorfoldLine {
    /// The profile spelling (`python`, `native`).
    pub fn name(self) -> &'static str {
        match self {
            TensorfoldLine::Python => "python",
            TensorfoldLine::Native => "native",
        }
    }

    /// Whether this is the Python line (serde skips it, so a Python profile
    /// serializes and fingerprints exactly as before the field existed).
    pub fn is_python(&self) -> bool {
        *self == TensorfoldLine::Python
    }

    pub fn is_native(self) -> bool {
        self == TensorfoldLine::Native
    }
}

/// ADR 0039 §2: the native release's executable, under the root's `bin`.
pub const EXECUTABLE: &str = "tensorfold-native";

/// ADR 0039 §2: the root's one-line version file.
pub const VERSION_FILE: &str = "VERSION";
/// ADR 0039 §2: at most this many bytes, one `MAJOR.MINOR.PATCH` line.
pub const VERSION_FILE_LIMIT: u64 = 32;
/// ADR 0039 §2: the publisher's checksum list over the rest of the root.
pub const CHECKSUMS_FILE: &str = "SHA256SUMS";
/// ADR 0039 §2: the capabilities document is read up to this many bytes.
pub const CAPABILITIES_LIMIT: usize = 64 * 1024;

/// ADR 0039 §2: the capabilities document `engine add` accepts.
pub const CAPABILITIES_SCHEMA: u64 = 1;
pub const CAPABILITIES_ENGINE: &str = "zig";
pub const CAPABILITIES_BACKEND: &str = "cuda";

/// ADR 0039 §2 step 3: every flag CapyCTL renders, each with the value it
/// renders where the document lists allowed values. A document whose
/// `serve.flags` lacks one is `engine_unsupported`, naming it.
pub const REQUIRED_FLAGS: &[(&str, Option<&str>)] = &[
    ("--host", None),
    ("--port", None),
    ("--name", None),
    ("--context", None),
    ("--parallel", None),
    ("--backend", Some("cuda")),
    ("--snapshot-dir", Some("none")),
    ("--no-update-check", None),
    ("--max-tokens", None),
    ("--thinking", None),
    ("--no-thinking", None),
    ("--no-drafts", None),
];

/// ADR 0039 §2 step 3: the environment variables the document's `env` must
/// list (CapyCTL renders the memory cap, §8).
pub const REQUIRED_ENV: &[&str] = &["TENSORFOLD_CUDA_MEMORY_LIMIT_GB"];

/// ADR 0039 §4: options CapyCTL renders or forbids on the native line,
/// compared by exact name (`--name=value` split at `=`): the native parser
/// matches names exactly and expands no abbreviation, so the prefix rule of
/// the Python line does not apply. Safety and ownership refusals: they apply
/// to every native version, certified or not.
pub const RESERVED_FLAGS: &[&str] = &[
    "--host",
    "--port",
    "--name",
    "--alias",
    "--backend",
    "--context",
    "--api-key",
    "--api-key-file",
    "--metrics-open",
    "--snapshot-dir",
    "--max-snapshots",
    "--no-update-check",
    // CapyCTL pins the GPU with CUDA_VISIBLE_DEVICES; the engine's default
    // ordinal 0 is the pinned device.
    "--device",
    // Two-Mac Metal pairing, a topology CapyCTL does not run.
    "--speed-up",
    // An extra page and `/stats` route on the engine's port.
    "--dashboard",
    // Served from 1.1.5 (Evidence §9). SPEC §6.5: the idle policy is
    // CapyCTL's; the engine's default 0 is left in place, nothing rendered.
    "--idle-unload",
    // CapyCTL is the engine's only client and reaches it at 127.0.0.1.
    "--allowed-host",
    "--allowed-origin",
    // Outbound web requests from the engine.
    "--cortheon",
    // §13: no group and no tensor parallelism on the native line.
    "--tp",
    "--rank",
    "--master",
    "--master-port",
];

/// The protected entry's own family, refused as on every line.
const RESERVED_FAMILIES: &[&str] = &["--capyctl-"];

/// ADR 0039 §4: the native spelling of each typed field.
pub const TYPED_OPTIONS: &[(&str, &str)] = &[
    ("--max-tokens", "tensorfold.max_tokens"),
    ("--thinking", "tensorfold.thinking"),
    ("--no-thinking", "tensorfold.thinking"),
];

/// ADR 0039 §6: CapyCTL renders no drafter on the native line.
pub const DRAFTER: &str = "--drafter";

/// ADR 0039 §4: the requests the native engine decodes together.
pub const PARALLEL: &str = "--parallel";

/// ADR 0039 §6: why a drafter is refused on the native line.
pub const DRAFTER_REFUSAL: &str = "capability_missing: CapyCTL renders no drafter on the \
     native line; TensorFold native drafts with the checkpoint's own MTP head";

/// ADR 0039 §6: why `kv_cache_dtype` is refused on the native line.
pub const KV_CACHE_DTYPE_REFUSAL: &str =
    "capability_missing: TensorFold native has no KV cache type option; its KV cache is BF16";

/// ADR 0039 §4 (owner decision 2026-10-27, question 4): the compaction
/// options, ordinary except under `retention: zero`.
pub const COMPACTION_OPTIONS: &[&str] = &["--compact-at", "--compact-keep", "--compact-memory"];

/// ADR 0039 §4: 1.1.5's options that write request state to disk. `--learn`
/// and `--recall` are refused with every option of their families
/// (`--learn-dir`, `--learn-conversations`, `--recall-dir`, ...): the owner's
/// decision names them `--learn*` and `--recall*`.
pub const ENGINE_DISK_OPTIONS: &[&str] = &["--record", "--learn", "--recall"];
const ENGINE_DISK_FAMILIES: &[&str] = &["--learn-", "--recall-"];

/// ADR 0039 §16: the zero-retention refusals (exit 5).
pub const RETENTION_ZERO_COMPACTION: &str = "retention_zero:compaction";
pub const RETENTION_ZERO_ENGINE_DISK: &str = "retention_zero:engine_disk";

/// ADR 0039 §14, §16: an owned native process listening off loopback.
pub const LISTENER_EXPOSED: &str = "tensorfold_listener_exposed";

/// ADR 0039 §5 (owner decision 2026-10-27, question 3): the engine's own
/// memory reserve, the smallest it takes on a unified device.
pub const MEMORY_RESERVE_GIB: &str = "2";

/// ADR 0039 §5: never carried, and refused as `--approve-env` names on a
/// native profile, each with why.
pub const REFUSED_ENV: &[(&str, &str)] = &[
    (
        "TENSORFOLD_API_KEY",
        "CapyCTL owns authentication (ADR 0023 §3)",
    ),
    (
        "TENSORFOLD_REQUEST_LOG",
        "it writes every request body to a file",
    ),
    (
        "TENSORFOLD_CUDA_KERNELS",
        "it would load kernels from outside the measured installation",
    ),
    ("TF_CUDA_DEVICE", "the pinned device is ordinal 0"),
    ("TF_CUDA_SEGMENTS", "`--segments` is the ordinary spelling"),
    (
        "BRAVE_SEARCH_API_KEY",
        "it is 1.1.5's web search, and `--cortheon` is reserved",
    ),
];

/// Why `name` is never carried into a native engine's environment, or `None`.
pub fn refused_env(name: &str) -> Option<&'static str> {
    REFUSED_ENV
        .iter()
        .find(|(refused, _)| *refused == name)
        .map(|(_, why)| *why)
}

fn reserved(name: &str) -> bool {
    RESERVED_FLAGS.contains(&name) || RESERVED_FAMILIES.iter().any(|f| name.starts_with(f))
}

/// The typed field `name` (exact) renders, or `None`.
pub fn typed_field(name: &str) -> Option<&'static str> {
    TYPED_OPTIONS
        .iter()
        .find(|(option, _)| *option == name)
        .map(|(_, field)| *field)
}

/// ADR 0039 §4, §6: the checks of a native pass-through vector, every name
/// compared exactly. Without `context` this is the check of host-fixed and
/// rendered pass-through arguments (reserved names, `--drafter`,
/// duplicates); with it, the deploy-time check of a deployment's extra
/// arguments adds the typed fields' spellings and duplicates of host-fixed
/// options. Anything else is ordinary: the binary refuses a flag it does not
/// serve, at start.
pub fn validate_args(
    args: &[String],
    context: Option<&ExtraArgsContext<'_>>,
) -> Result<(), ProfileArgError> {
    let mut seen = BTreeSet::new();
    for option in parse_options(args)? {
        let name = option.name;
        if reserved(&name) {
            return Err(ProfileArgError::Reserved(name));
        }
        if name == DRAFTER {
            return Err(ProfileArgError::NativeDrafter(name));
        }
        if let Some(context) = context {
            if let Some(field) = typed_field(&name) {
                return Err(ProfileArgError::TypedField {
                    option: name,
                    field: field.to_owned(),
                });
            }
            if context.host_fixed.contains(&name) {
                return Err(ProfileArgError::Duplicate(name));
            }
        }
        if !seen.insert(name.clone()) {
            return Err(ProfileArgError::Duplicate(name));
        }
    }
    Ok(())
}

/// ADR 0039 §4 (ADR 0023 §4 amendment unchanged): `--parallel` among `args`
/// by exact name: `Some` with the last occurrence's value, `None` when absent.
pub fn parallel(args: &[String]) -> Result<Option<Option<String>>, ProfileArgError> {
    Ok(parse_options(args)?
        .into_iter()
        .rev()
        .find(|option| option.name == PARALLEL)
        .map(|option| option.value))
}

/// ADR 0039 §4 (owner decision 2026-10-27, question 4): under `retention:
/// zero`, the first option of `args` that compaction or a disk write needs,
/// as `(code, option)`: `retention_zero:compaction` for `--compact-at`,
/// `--compact-keep` and `--compact-memory`, `retention_zero:engine_disk` for
/// `--record`, `--learn*` and `--recall*`. `None` when `args` has neither;
/// a malformed vector is left to the argument checks.
pub fn retention_zero_refusal(args: &[String]) -> Option<(&'static str, String)> {
    parse_options(args).ok()?.into_iter().find_map(|option| {
        let name = option.name;
        if COMPACTION_OPTIONS.contains(&name.as_str()) {
            Some((RETENTION_ZERO_COMPACTION, name))
        } else if ENGINE_DISK_OPTIONS.contains(&name.as_str())
            || ENGINE_DISK_FAMILIES.iter().any(|f| name.starts_with(f))
        {
            Some((RETENTION_ZERO_ENGINE_DISK, name))
        } else {
            None
        }
    })
}

/// The refusal text of [`retention_zero_refusal`]'s answer: the code, the
/// option and the fix.
pub fn retention_zero_message(code: &str, option: &str) -> String {
    let what = if code == RETENTION_ZERO_COMPACTION {
        "compacts conversations"
    } else {
        "writes request state to disk"
    };
    format!(
        "{code}: `{option}` {what}, which `retention: zero` does not allow on TensorFold \
         native; remove it, or run without `retention: zero`"
    )
}

/// ADR 0039 §3 (owner decision 2026-10-27, question 1): a native version the
/// owner certifies, and the live rows that certify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Certification {
    pub version: &'static str,
    /// The live rows (ADR 0039, "Live qualification").
    pub rows: &'static str,
    /// Whether the rows have passed. ADR 0039 §3: a version joins the
    /// certified set once its rows pass; until then it runs uncertified and
    /// says which rows certify it.
    pub passed: bool,
}

/// ADR 0039 §3: 1.0.0 (TF1 to TF10) and 1.1.5 (TF11 to TF14). Neither row
/// set has run (no lab host was reachable when this was written); a row set
/// that passes sets its `passed`.
pub const CERTIFICATIONS: &[Certification] = &[
    Certification {
        version: "1.0.0",
        rows: "TF1 to TF10",
        passed: false,
    },
    Certification {
        version: "1.1.5",
        rows: "TF11 to TF14",
        passed: false,
    },
];

/// ADR 0039 §3: whether `version` is a certified native version.
pub fn is_certified(version: &str) -> bool {
    is_certified_in(CERTIFICATIONS, version)
}

/// [`is_certified`] against `table` (a test states its own set).
pub fn is_certified_in(table: &[Certification], version: &str) -> bool {
    table
        .iter()
        .any(|entry| entry.passed && entry.version == version)
}

/// The certified versions, for the notes ("1.0.0 and 1.1.5").
fn certified_names(table: &[Certification]) -> String {
    let names: Vec<&str> = table.iter().map(|entry| entry.version).collect();
    match names.as_slice() {
        [] => "no version".to_owned(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// ADR 0039 §3: what `engine list`, `status deployment` and `validate
/// config` say about a native version that is not certified, or `None` for a
/// certified one. Never a refusal.
pub fn version_note(version: &str) -> Option<String> {
    version_note_in(CERTIFICATIONS, version)
}

/// [`version_note`] against `table`.
pub fn version_note_in(table: &[Certification], version: &str) -> Option<String> {
    if is_certified_in(table, version) {
        return None;
    }
    let pending = table.iter().find(|entry| entry.version == version);
    Some(match pending {
        Some(entry) => format!(
            "not certified yet: TensorFold native {version} is certified once its live rows \
             ({}) pass",
            entry.rows
        ),
        None => format!(
            "not certified: TensorFold native {version} is a custom setup; CapyCTL certifies \
             TensorFold native {}",
            certified_names(table)
        ),
    })
}

/// ADR 0039 §7: the one certified checkpoint, under its repository id and
/// the earlier one that redirects to it (the same revision, the same
/// checkpoint).
pub const CERTIFIED_CHECKPOINT_REPOS: &[&str] = &[
    "TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit",
    "Vontra/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit",
];
pub const CERTIFIED_CHECKPOINT_REVISION: &str = "d9d758fb83953437f7263256b0d96157e2a348b8";

/// ADR 0039 §7: the note an uncertified checkpoint runs with.
pub const CHECKPOINT_NOTE: &str = "not certified: TensorFold native certifies only Nemotron \
     3.5 Lightning at revision d9d758f on a GB10";

/// ADR 0039 §7: whether a Hugging Face source `repo` at `revision` is the
/// certified checkpoint (repository ids compare without case, as the Hub's).
pub fn is_certified_checkpoint(repo: &str, revision: &str) -> bool {
    revision == CERTIFIED_CHECKPOINT_REVISION
        && CERTIFIED_CHECKPOINT_REPOS
            .iter()
            .any(|certified| certified.eq_ignore_ascii_case(repo))
}

/// ADR 0039 §2, §7: what `engine add` records on a native profile from the
/// installation: the capabilities document's families with their weight
/// formats, and whether the root carried the publisher's checksum list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRegistration {
    /// Family (`model_type`) to the weight formats it reads, as the
    /// capabilities document lists them (1.0.0 on Linux: `nemotron_h`
    /// reading `mlx-q4g64`).
    #[serde(default)]
    pub families: BTreeMap<String, Vec<String>>,
    /// Whether the root held `SHA256SUMS` and every listed file matched it;
    /// `false` when the root had none (`engine list` says the publisher's
    /// list was absent).
    pub checksums_verified: bool,
}

/// ADR 0039 §7: a checkpoint's family and weight format from its
/// `config.json`: `model_type` and the format name the engine derives from
/// `quantization` (`mlx-q<bits>g<group_size>`, `cli/hub.zig:117-128`).
pub fn checkpoint_format(config: &serde_json::Value) -> (Option<String>, Option<String>) {
    let family = config
        .get("model_type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let quantization = config.get("quantization");
    let number = |key: &str| {
        quantization
            .and_then(|q| q.get(key))
            .and_then(serde_json::Value::as_u64)
    };
    let format = match (number("bits"), number("group_size")) {
        (Some(bits), Some(group)) => Some(format!("mlx-q{bits}g{group}")),
        _ => None,
    };
    (family, format)
}

/// ADR 0039 §7: the notes a native deployment runs with, from its source
/// (`repo` and `revision` when it is a Hugging Face source), the checkpoint's
/// `config.json` when this machine can read it, and the profile's recorded
/// families. Empty for the certified checkpoint. Never a refusal.
pub fn checkpoint_notes(
    hf: Option<(&str, &str)>,
    config: Option<&serde_json::Value>,
    registration: Option<&NativeRegistration>,
) -> Vec<String> {
    let mut notes = Vec::new();
    if hf.is_some_and(|(repo, revision)| is_certified_checkpoint(repo, revision)) {
        return notes;
    }
    notes.push(CHECKPOINT_NOTE.to_owned());
    let (Some(config), Some(registration)) = (config, registration) else {
        return notes;
    };
    let (family, format) = checkpoint_format(config);
    let listed = family
        .as_deref()
        .and_then(|family| registration.families.get(family));
    let mut missing = Vec::new();
    if let Some(family) = family.as_deref().filter(|_| listed.is_none()) {
        missing.push(format!("family {family}"));
    }
    if let (Some(format), Some(formats)) = (format.as_deref(), listed) {
        if !formats.iter().any(|listed| listed == format) {
            missing.push(format!("format {format}"));
        }
    }
    if !missing.is_empty() {
        notes.push(format!(
            "this installation does not list {}; the engine is expected to refuse it at start",
            missing.join(" or ")
        ));
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_owned()).collect()
    }

    // T41
    #[test]
    fn reserved_options_are_refused_by_exact_name_and_with_equals() {
        for name in RESERVED_FLAGS {
            assert_eq!(
                validate_args(&args(&[name, "x"]), None),
                Err(ProfileArgError::Reserved((*name).to_owned())),
                "{name}"
            );
            let joined = format!("{name}=x");
            assert_eq!(
                validate_args(&args(&[&joined]), None),
                Err(ProfileArgError::Reserved((*name).to_owned())),
                "{joined}"
            );
        }
        // No prefix rule: an abbreviation is the binary's to refuse.
        validate_args(
            &args(&["--cont", "8", "--idle", "3", "--compact-at", "auto"]),
            None,
        )
        .unwrap();
        assert_eq!(
            validate_args(&args(&["--idle-unload", "60"]), None),
            Err(ProfileArgError::Reserved("--idle-unload".into()))
        );
    }

    // T41
    #[test]
    fn a_drafter_is_refused_and_no_drafts_is_ordinary() {
        let refused = validate_args(&args(&["--drafter", "none"]), None).unwrap_err();
        assert!(
            refused.to_string().starts_with("capability_missing"),
            "{refused}"
        );
        validate_args(&args(&["--no-drafts", "--loop-guard", "on"]), None).unwrap();
    }

    // T41
    #[test]
    fn typed_spellings_and_host_fixed_duplicates_are_refused_in_extra_args() {
        let host_fixed: BTreeSet<String> = ["--segments".to_owned()].into();
        let approved = BTreeSet::new();
        let context = ExtraArgsContext {
            engine: crate::engine_policy::Engine::Tensorfold,
            sleep_mode: false,
            approved_options: &approved,
            approved_paths: &[],
            checkpoint_root: None,
            host_fixed: &host_fixed,
        };
        assert!(matches!(
            validate_args(&args(&["--no-thinking"]), Some(&context)),
            Err(ProfileArgError::TypedField { .. })
        ));
        assert_eq!(
            validate_args(&args(&["--segments", "2"]), Some(&context)),
            Err(ProfileArgError::Duplicate("--segments".into()))
        );
        assert_eq!(
            validate_args(&args(&["--loop-guard", "a", "--loop-guard", "b"]), None),
            Err(ProfileArgError::Duplicate("--loop-guard".into()))
        );
    }

    // T41
    #[test]
    fn zero_retention_refuses_compaction_and_disk_writing_options() {
        for option in COMPACTION_OPTIONS {
            assert_eq!(
                retention_zero_refusal(&args(&[option, "x"])),
                Some((RETENTION_ZERO_COMPACTION, (*option).to_owned()))
            );
        }
        for option in [
            "--record",
            "--learn",
            "--learn-dir",
            "--learn-conversations",
            "--learn-gib",
            "--recall",
            "--recall-dir",
            "--recall-read-only",
        ] {
            assert_eq!(
                retention_zero_refusal(&args(&[option])),
                Some((RETENTION_ZERO_ENGINE_DISK, option.to_owned())),
                "{option}"
            );
        }
        assert_eq!(
            retention_zero_refusal(&args(&["--loop-guard", "on", "--no-drafts"])),
            None
        );
        let message = retention_zero_message(RETENTION_ZERO_COMPACTION, "--compact-at");
        assert!(message.starts_with("retention_zero:compaction: `--compact-at`"));
    }

    // T41
    #[test]
    fn certified_versions_and_uncertified_notes() {
        let passed = [
            Certification {
                version: "1.0.0",
                rows: "TF1 to TF10",
                passed: true,
            },
            Certification {
                version: "1.1.5",
                rows: "TF11 to TF14",
                passed: true,
            },
        ];
        assert!(is_certified_in(&passed, "1.0.0"));
        assert!(is_certified_in(&passed, "1.1.5"));
        assert_eq!(version_note_in(&passed, "1.1.5"), None);
        for other in ["1.0.3", "1.2.0", "1.1.4"] {
            assert!(!is_certified_in(&passed, other));
            let note = version_note_in(&passed, other).unwrap();
            assert!(note.contains("1.0.0 and 1.1.5"), "{note}");
            assert!(note.starts_with("not certified"), "{note}");
        }
        // Until its rows pass, a certified-to-be version says which rows.
        let note = version_note("1.0.0").unwrap();
        assert!(note.contains("TF1 to TF10"), "{note}");
    }

    // T41
    #[test]
    fn checkpoint_notes_name_the_certified_checkpoint_and_unlisted_formats() {
        let registration = NativeRegistration {
            families: [("nemotron_h".to_owned(), vec!["mlx-q4g64".to_owned()])].into(),
            checksums_verified: true,
        };
        for repo in CERTIFIED_CHECKPOINT_REPOS {
            assert!(checkpoint_notes(
                Some((repo, CERTIFIED_CHECKPOINT_REVISION)),
                None,
                Some(&registration)
            )
            .is_empty());
        }
        let nemotron = serde_json::json!({
            "model_type": "nemotron_h",
            "quantization": {"bits": 4, "group_size": 64, "mode": "affine"},
        });
        // A fine-tune of a listed family and format: the note alone.
        assert_eq!(
            checkpoint_notes(
                Some(("me/tune", "abc")),
                Some(&nemotron),
                Some(&registration)
            ),
            vec![CHECKPOINT_NOTE.to_owned()]
        );
        let qwen = serde_json::json!({"model_type": "qwen3"});
        let notes = checkpoint_notes(
            Some(("Qwen/Qwen3-4B", "x")),
            Some(&qwen),
            Some(&registration),
        );
        assert_eq!(notes.len(), 2);
        assert!(notes[1].contains("family qwen3"), "{notes:?}");
        let eight_bit = serde_json::json!({
            "model_type": "nemotron_h", "quantization": {"bits": 8, "group_size": 64},
        });
        let notes = checkpoint_notes(None, Some(&eight_bit), Some(&registration));
        assert!(notes[1].contains("format mlx-q8g64"), "{notes:?}");
    }

    #[test]
    fn the_line_defaults_to_python_and_spells_lowercase() {
        assert_eq!(TensorfoldLine::default(), TensorfoldLine::Python);
        assert_eq!(
            serde_json::to_value(TensorfoldLine::Native).unwrap(),
            serde_json::json!("native")
        );
        assert!(refused_env("TENSORFOLD_REQUEST_LOG").is_some());
        assert_eq!(refused_env("HF_HOME"), None);
    }
}
