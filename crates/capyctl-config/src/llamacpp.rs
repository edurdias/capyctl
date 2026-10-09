//! ADR 0029: llama.cpp's `llama-server`, the fourth engine. Registration
//! constants shared by `engine add`, the roles' own installations and the
//! listings: the executable's name, the version line `--version` writes to
//! standard error, the build fingerprint made from it, and the machine-wide
//! configuration file that refuses an installation. The option tables of ADR
//! 0029 §6 (read from llama-server 0.6.0, `common/arg.cpp`), the cache types
//! and the hidden inputs of the launch environment live here too; the
//! exact-name matcher that applies them is `engine_policy.rs`'s.
use crate::engine_policy::Sensitivity;
use std::path::{Component, Path, PathBuf};

/// ADR 0029 §2: the file detection and `engine add` look for.
pub const EXECUTABLE: &str = "llama-server";

/// ADR 0029 §2: the root `/etc/llama.cpp/config.ini` is read under. Tests
/// name their own root so the result does not depend on the machine.
pub const SYSTEM_ROOT: &str = "/";

/// ADR 0029 §2, §6: llama.cpp fills every option the command line leaves unset
/// from this file, so CapyCTL could not see what the engine runs with.
pub const SYSTEM_CONFIG_FILE: &str = "etc/llama.cpp/config.ini";

/// ADR 0029 §2: each release's tag commit, so a build of the tag configured
/// without `-DLLAMA_BUILD_IS_DEV=OFF` (which reports `<v>-dev`) counts as that
/// release.
pub const RELEASE_COMMITS: &[(&str, &str)] = &[("0.6.0", "d812350")];

/// Git's shortest abbreviation; a commit compared with a tag's is at least
/// this long.
const MIN_COMMIT_LEN: usize = 7;
const MAX_VERSION_LEN: usize = 128;
const MAX_COMMIT_LEN: usize = 64;

/// `/etc/llama.cpp/config.ini` under `root`.
pub fn system_config_file(root: &Path) -> PathBuf {
    root.join(SYSTEM_CONFIG_FILE)
}

/// ADR 0029 §2: why an installation on a machine with the system
/// configuration file is refused, or `None` when there is none. Any entry at
/// that path counts (`symlink_metadata`, not followed): llama.cpp would read it.
pub fn system_config_refusal(root: &Path) -> Option<String> {
    let file = system_config_file(root);
    std::fs::symlink_metadata(&file).ok().map(|_| {
        format!(
            "{} exists; llama.cpp fills every option CapyCTL leaves unset from it, so \
             llama-server would run with settings CapyCTL cannot see; remove it to use \
             llama.cpp on this machine",
            file.display()
        )
    })
}

/// ADR 0029 §2: what `llama-server --version` reports, as
/// `version: <v> (build <n>, commit <h>)`. The build number counts the
/// commits in the clone, so it is not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamacppBuild {
    pub version: String,
    pub commit: String,
}

fn version_token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_VERSION_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn commit_token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_COMMIT_LEN
        && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl LlamacppBuild {
    /// The `version:` line of `--version`'s standard error. Every other line
    /// (the compiler line, backend notices) is ignored; a malformed version
    /// line, or none, is `None`.
    pub fn parse_version_output(text: &str) -> Option<Self> {
        text.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("version: "))
            .and_then(Self::parse_version_line)
    }

    /// `<v> (build <n>, commit <h>)`.
    fn parse_version_line(rest: &str) -> Option<Self> {
        let (version, rest) = rest.split_once(" (build ")?;
        let (build, rest) = rest.split_once(", commit ")?;
        let commit = rest.strip_suffix(')')?;
        (version_token(version)
            && !build.is_empty()
            && build.bytes().all(|b| b.is_ascii_digit())
            && commit_token(commit))
        .then(|| Self {
            version: version.to_owned(),
            commit: commit.to_owned(),
        })
    }

    /// ADR 0029 §2: the profile's `build_fingerprint`, `<v>+<h>`.
    pub fn fingerprint(&self) -> String {
        format!("{}+{}", self.version, self.commit)
    }

    /// A `build_fingerprint` read back. One without a commit (a role's
    /// stated fingerprint, or a version read from a library name) is the
    /// version alone with an empty commit.
    pub fn from_fingerprint(fingerprint: &str) -> Option<Self> {
        let (version, commit) = fingerprint.split_once('+').unwrap_or((fingerprint, ""));
        (version_token(version) && (commit.is_empty() || commit_token(commit))).then(|| Self {
            version: version.to_owned(),
            commit: commit.to_owned(),
        })
    }

    /// ADR 0029 §2: the release this build counts as: its version, or a
    /// `<v>-dev` build of `<v>`'s tag commit as `<v>`.
    pub fn release(&self) -> &str {
        match self.version.strip_suffix("-dev") {
            Some(base) if self.is_tag_commit(base) => base,
            _ => &self.version,
        }
    }

    fn is_tag_commit(&self, release: &str) -> bool {
        let commit = self.commit.to_ascii_lowercase();
        commit.len() >= MIN_COMMIT_LEN
            && RELEASE_COMMITS.iter().any(|(version, tag)| {
                *version == release && (commit.starts_with(tag) || tag.starts_with(&commit))
            })
    }
}

/// ADR 0029 §9 (plan slice L4): until CapyCTL derives a llama.cpp
/// deployment's memory request from its GGUF header, the deployment states
/// it.
pub const NEEDS_RESOURCES: &str = "a llama.cpp deployment states `resources` (the short \
     form or its phases): deriving its memory request from the GGUF header is not in this \
     release yet";

/// ADR 0029 §6: options CapyCTL renders on every launch (`--mmproj` when the
/// deployment names a projector). Each row is one llama-server 0.6.0 option
/// with every long spelling its parser accepts: aliases and negative forms.
pub const RESERVED_RENDERED: &[&[&str]] = &[
    &["--host"],
    &["--port"],
    &["--model"],
    &["--alias"],
    &["--ctx-size"],
    &["--parallel"],
    &["--kv-unified", "--no-kv-unified"],
    &["--kv-unified-per-slot"],
    &["--gpu-layers", "--n-gpu-layers"],
    &["--cache-type-k"],
    &["--cache-type-v"],
    &["--fit"],
    &["--fit-target"],
    &["--fit-ctx"],
    &["--cache-ram"],
    &["--context-shift", "--no-context-shift"],
    &["--metrics"],
    &["--slots", "--no-slots"],
    &["--offline"],
    &["--ui", "--webui", "--no-ui", "--no-webui"],
    &["--cors-origins"],
    &["--cors-methods"],
    &["--cors-headers"],
    &["--cors-credentials", "--no-cors-credentials"],
    &["--mmproj"],
];

/// ADR 0029 §6: options CapyCTL never renders and a deployment or host may
/// not pass: the engine key, its own idle unload, router mode, RPC, slot
/// files, other listeners and routes, logs (request logging stays off, SPEC
/// §13.3), model sources and presets (CapyCTL materializes checkpoints, ADR
/// 0008), the built-in agent surface, the device choice (one GPU, chosen by
/// CapyCTL, ADR 0019), embedding and reranking (v1 serves chat), and the
/// options that print and exit.
pub const RESERVED_NEVER_RENDERED: &[&[&str]] = &[
    &["--api-key"],
    &["--api-key-file"],
    &["--sleep-idle-seconds"],
    &["--models-dir"],
    &["--models-preset"],
    &["--models-max"],
    &["--models-autoload", "--no-models-autoload"],
    &["--rpc"],
    &["--slot-save-path"],
    &["--cache-idle-slots", "--no-cache-idle-slots"],
    &["--props"],
    &["--reuse-port"],
    &["--path"],
    &["--api-prefix"],
    &["--ssl-key-file"],
    &["--ssl-cert-file"],
    &["--log-file"],
    &["--log-prompts-dir"],
    &["--verbose", "--log-verbose"],
    &["--verbosity", "--log-verbosity"],
    &["--hf-repo"],
    &["--hf-file"],
    &["--hf-token"],
    &["--model-url"],
    &["--docker-repo"],
    &["--mmproj-url"],
    &["--spec-draft-hf", "--hf-repo-draft"],
    &["--embd-gemma-default"],
    &["--fim-qwen-1.5b-default"],
    &["--fim-qwen-3b-default"],
    &["--fim-qwen-7b-default"],
    &["--fim-qwen-7b-spec"],
    &["--fim-qwen-14b-spec"],
    &["--fim-qwen-30b-default"],
    &["--gpt-oss-20b-default"],
    &["--gpt-oss-120b-default"],
    &["--vision-gemma-4b-default"],
    &["--vision-gemma-12b-default"],
    &["--spec-default"],
    &["--tools"],
    &["--tools-runtime"],
    &["--agent", "--no-agent"],
    &["--mcp-servers-config"],
    &["--mcp-servers-json"],
    &[
        "--ui-mcp-proxy",
        "--webui-mcp-proxy",
        "--no-ui-mcp-proxy",
        "--no-webui-mcp-proxy",
    ],
    &["--device"],
    &["--split-mode"],
    &["--tensor-split"],
    &["--main-gpu"],
    &["--spec-draft-device", "--device-draft"],
    &["--mmproj-device"],
    &["--embedding", "--embeddings"],
    &["--rerank", "--reranking"],
    &["--pooling"],
    &["--help", "--usage"],
    &["--version"],
    &["--list-devices"],
    &["--cache-list"],
    &["--completion-bash"],
];

/// ADR 0029 §5, §9: the reserved options a typed field renders, so a
/// refusal names the field to set instead.
pub const TYPED_OPTIONS: &[(&str, &str)] = &[
    ("--ctx-size", "context_length"),
    ("--parallel", "max_concurrent_requests"),
    ("--cache-type-k", "kv_cache_dtype"),
    ("--cache-type-v", "kv_cache_dtype"),
    ("--gpu-layers", "llamacpp.n_gpu_layers"),
    ("--n-gpu-layers", "llamacpp.n_gpu_layers"),
    ("--model", "llamacpp.gguf_file"),
    ("--mmproj", "llamacpp.mmproj_file"),
];

/// ADR 0029 §6, §8 (ADR 0014 §8): options the host approves by name. The
/// path options' values must lie inside `security.approved_paths`;
/// `--video-ffmpeg-dir` names programs llama-server runs.
pub const SENSITIVE: &[(&[&str], Sensitivity)] = &[
    (&["--spec-draft-model", "--model-draft"], PATH),
    (&["--lora"], PATH),
    (&["--lora-scaled"], PATH),
    (&["--control-vector"], PATH),
    (&["--control-vector-scaled"], PATH),
    (&["--chat-template-file"], PATH),
    (&["--grammar-file"], PATH),
    (&["--json-schema-file"], PATH),
    (&["--lookup-cache-static"], PATH),
    (&["--lookup-cache-dynamic"], PATH),
    (&["--media-path"], PATH),
    (&["--video-ffmpeg-dir"], Sensitivity::Code),
];

const PATH: Sensitivity = Sensitivity::Path {
    checkpoint_exempt: false,
};

/// Path options whose value is a comma-separated list (`parse_csv_row`).
const PATH_LISTS: &[&str] = &["--lora", "--control-vector"];
/// Path options whose value is a comma-separated list of `<path>:<scale>`.
const SCALED_PATH_LISTS: &[&str] = &["--lora-scaled", "--control-vector-scaled"];

/// The row of `table` naming `name` (an exact, normalized long name).
fn row_of<'a>(table: &'a [&'a [&'a str]], name: &str) -> Option<&'a [&'a str]> {
    table.iter().copied().find(|row| row.contains(&name))
}

/// ADR 0029 §6: whether `name` (normalized: lower case, `_` spelled `-`, no
/// `=value`) is a reserved llama-server option in any of its spellings.
pub fn is_reserved(name: &str) -> bool {
    row_of(RESERVED_RENDERED, name).is_some() || row_of(RESERVED_NEVER_RENDERED, name).is_some()
}

/// ADR 0014 open issue 5: llama-server options whose name has a sensitive
/// shape but whose value is none of those things: `--no-host` turns off a
/// host memory buffer and opens nothing.
pub const ORDINARY_SHAPED: &[&str] = &["--no-host"];

/// The typed field a reserved option renders, when one does.
pub fn typed_field(name: &str) -> Option<&'static str> {
    TYPED_OPTIONS
        .iter()
        .find(|(option, _)| *option == name)
        .map(|(_, field)| *field)
}

/// ADR 0014 §8: the sensitivity of a listed option in any of its spellings,
/// and every spelling of it (an approval of one spelling approves the option).
pub fn sensitive(name: &str) -> Option<(&'static [&'static str], Sensitivity)> {
    SENSITIVE
        .iter()
        .find(|(row, _)| row.contains(&name))
        .map(|(row, kind)| (*row, *kind))
}

/// One option however it is spelled: its table row's first name, else the
/// name with a `--no-` negation folded onto its base (llama-server's negative
/// forms are the same option), so two spellings of one option are a
/// duplicate.
pub fn option_key(name: &str) -> String {
    for table in [RESERVED_RENDERED, RESERVED_NEVER_RENDERED] {
        if let Some(row) = row_of(table, name) {
            return row[0].to_owned();
        }
    }
    if let Some((row, _)) = sensitive(name) {
        return row[0].to_owned();
    }
    match name.strip_prefix("--no-") {
        Some(base) => format!("--{base}"),
        None => name.to_owned(),
    }
}

/// ADR 0014 §8, ADR 0029 §8: the paths a path option's value names, as
/// llama-server reads them: one path, a comma-separated list (`--lora`,
/// `--control-vector`), or a list of `<path>:<scale>` (`--lora-scaled`,
/// `--control-vector-scaled`). `None` when the value cannot be read the way
/// llama-server reads it without doubt (a quote, which its list parser
/// interprets, or a scaled item without exactly one `:`): it is then refused.
pub fn path_values<'a>(name: &str, value: &'a str) -> Option<Vec<&'a str>> {
    let list = PATH_LISTS.contains(&name);
    let scaled = SCALED_PATH_LISTS.contains(&name);
    if !list && !scaled {
        return Some(vec![value]);
    }
    if value.contains('"') {
        return None;
    }
    value
        .split(',')
        .map(|item| match scaled {
            false => Some(item),
            true => match item.split(':').collect::<Vec<_>>().as_slice() {
                [path, _] => Some(*path),
                _ => None,
            },
        })
        .collect()
}

/// ADR 0029 §5: the KV cache types llama-server 0.6.0 accepts for
/// `--cache-type-k` and `--cache-type-v`.
pub const CACHE_TYPES: &[&str] = &[
    "f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1",
];

/// ADR 0029 §5: CapyCTL's cache type when the deployment names none.
pub const DEFAULT_CACHE_TYPE: &str = "f16";

/// llama.cpp rounds each slot's window up to a multiple of this.
pub const SLOT_ALIGNMENT: u32 = 256;

/// ADR 0029 §5: the `--ctx-size` for `context_length` tokens in each of
/// `slots` slots, `pad256(context_length) × slots`, so each slot of a cache
/// that is not unified holds the declared window. `None` when it does not
/// fit llama-server's `int` or a count is zero.
pub fn slot_pool_tokens(context_length: u32, slots: u32) -> Option<u32> {
    if context_length == 0 || slots == 0 {
        return None;
    }
    let window =
        u64::from(context_length).div_ceil(u64::from(SLOT_ALIGNMENT)) * u64::from(SLOT_ALIGNMENT);
    window
        .checked_mul(u64::from(slots))
        .filter(|tokens| *tokens <= i32::MAX as u64)
        .and_then(|tokens| u32::try_from(tokens).ok())
}

/// ADR 0029 §6: environment names llama-server reads as options or as its
/// configuration and cache locations. A profile or deployment `env` naming
/// one is refused, whatever `approved_env` says: `LLAMA_ARG_*` and
/// `LLAMA_API_KEY` set options, `LLAMA_SERVER_SLOTS_DEBUG` exposes prompts in
/// `/slots`, and `LLAMA_CACHE`, `XDG_CONFIG_HOME` and `HOME` would move the
/// directories CapyCTL points at its own empty ones.
pub const REFUSED_ENV_NAMES: &[&str] = &[
    "LLAMA_API_KEY",
    "LLAMA_SERVER_SLOTS_DEBUG",
    "LLAMA_CACHE",
    "XDG_CONFIG_HOME",
    "HOME",
];
/// The prefix of every variable llama-server reads as an option.
pub const REFUSED_ENV_PREFIX: &str = "LLAMA_ARG_";

/// Whether a profile or deployment environment may not name `name` on a
/// llama.cpp profile (compared upper-cased, as `engine_env::is_owned` is).
pub fn refused_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with(REFUSED_ENV_PREFIX) || REFUSED_ENV_NAMES.contains(&upper.as_str())
}

/// The first name of a llama.cpp profile's or deployment's `env` that is
/// refused, as the closed reason `engine_env_reserved:<name>`.
pub fn refused_env<'a>(names: impl IntoIterator<Item = &'a String>) -> Option<String> {
    names
        .into_iter()
        .find(|name| refused_env_name(name))
        .map(|name| format!("engine_env_reserved:{name}"))
}

/// The longest file name `gguf_file` or `mmproj_file` may give.
const MAX_CHECKPOINT_FILE_BYTES: usize = 1024;

/// ADR 0029 §9: a file `engine_config.llamacpp.gguf_file` or `.mmproj_file`
/// names: a relative `.gguf` path inside the checkpoint, compared lexically
/// as written, so no `..`, `.`, empty or absolute component can leave it
/// (and a value names one file one way).
pub fn checkpoint_file(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    let inside = !value.is_empty()
        && value.len() <= MAX_CHECKPOINT_FILE_BYTES
        && !value.contains('\0')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !inside {
        return Err(format!(
            "`{value}` must be a relative path inside the checkpoint, without `.` or `..`"
        ));
    }
    if !value.to_ascii_lowercase().ends_with(".gguf") {
        return Err(format!("`{value}` must name a `.gguf` file"));
    }
    Ok(path.to_path_buf())
}
