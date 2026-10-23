//! T45 (ADR 0033 §1): LMCache measured for `engine add` and checked by shape
//! at launch, against fake environments built from excerpts of the real
//! LMCache 0.5.6, vLLM 0.32.0 and SGLang 0.5.22 sources
//! (`tests/fixtures/lmcache`). CPU tests only; they are not qualification.

use capyctl_agent::installation::InstallationMeasurer;
use capyctl_agent::lmcache_install::{
    check_lmcache, measure_lmcache, LmcacheCheckFailure, LmcacheInstall,
};
use capyctl_config::engine_policy::Engine;
use capyctl_config::lmcache::{LmcacheOffReason, LmcacheRegistration};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lmcache");

const ENTRY_POINTS: &str = "[console_scripts]\n\
lmcache = lmcache.cli.main:main\n\
lmcache_controller = lmcache.v1.api_server.__main__:main\n\
lmcache_server = lmcache.v1.server.__main__:main\n";

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A virtual environment with the engine and LMCache 0.5.6. Every program in
/// `bin` (the interpreter included) records that it ran and fails, so a
/// check that executed anything is caught.
struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    site: PathBuf,
    engine: Engine,
    measurer: InstallationMeasurer,
}

impl Env {
    fn new(engine: Engine) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("venv");
        let site = root.join("lib/python3.12/site-packages");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let marker = root.join("executed");
        for program in ["python3", "vllm", "lmcache"] {
            let path = root.join("bin").join(program);
            std::fs::write(
                &path,
                format!("#!/bin/sh\ntouch '{}'\nexit 97\n", marker.display()),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let engine_fixture = match engine {
            Engine::Vllm => "vllm-0.32.0",
            Engine::Sglang => "sglang-0.5.22",
            _ => "vllm-0.32.0",
        };
        copy_tree(&Path::new(FIXTURES).join(engine_fixture), &site);
        copy_tree(&Path::new(FIXTURES).join("lmcache-0.5.6"), &site);
        let env = Self {
            _dir: dir,
            root,
            site,
            engine,
            measurer: InstallationMeasurer::new(),
        };
        env.set_version("0.5.6");
        env
    }

    fn executable(&self) -> PathBuf {
        match self.engine {
            Engine::Vllm => self.root.join("bin/vllm"),
            _ => self.root.join("bin/python3"),
        }
    }

    /// The dist-info of `version`, the only one.
    fn set_version(&self, version: &str) {
        for entry in std::fs::read_dir(&self.site).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            if name.starts_with("lmcache-") && name.ends_with(".dist-info") {
                std::fs::remove_dir_all(self.site.join(name.as_ref())).unwrap();
            }
        }
        let dist = self.site.join(format!("lmcache-{version}.dist-info"));
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(
            dist.join("METADATA"),
            format!("Metadata-Version: 2.4\nName: lmcache\nVersion: {version}\n"),
        )
        .unwrap();
        std::fs::write(dist.join("entry_points.txt"), ENTRY_POINTS).unwrap();
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.site.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Replace `from` (which must be there) by `to` in one file.
    fn edit(&self, relative: &str, from: &str, to: &str) {
        let path = self.site.join(relative);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{relative} holds no {from:?}");
        std::fs::write(&path, text.replace(from, to)).unwrap();
    }

    fn remove(&self, relative: &str) {
        std::fs::remove_file(self.site.join(relative)).unwrap();
    }

    fn register(&self) -> LmcacheRegistration {
        let measured = measure_lmcache(&self.measurer, &self.executable())
            .unwrap()
            .expect("LMCache is installed");
        self.assert_nothing_ran();
        measured.registration()
    }

    fn check(
        &self,
        registered: Option<&LmcacheRegistration>,
    ) -> Result<LmcacheInstall, LmcacheCheckFailure> {
        let result = check_lmcache(&self.measurer, self.engine, &self.executable(), registered);
        self.assert_nothing_ran();
        result
    }

    /// The check with a registration of what is there now.
    fn check_registered(&self) -> Result<LmcacheInstall, LmcacheCheckFailure> {
        let registration = self.register();
        self.check(Some(&registration))
    }

    fn assert_nothing_ran(&self) {
        assert!(
            !self.root.join("executed").exists(),
            "the check executed a program of the environment"
        );
    }
}

fn check_failed(env: &Env) -> String {
    let failure = env.check_registered().unwrap_err();
    assert_eq!(failure.reason, LmcacheOffReason::CheckFailed, "{failure}");
    assert!(
        !failure.detail.contains(&*env.root.to_string_lossy()),
        "{failure}"
    );
    failure.detail
}

// T45 (ADR 0033 §1): vLLM 0.32.0 with LMCache 0.5.6 passes, reports the
// version and the digest `engine add` records, and no drift.
#[test]
fn lmcache_check_passes_on_vllm_with_lmcache_056() {
    let env = Env::new(Engine::Vllm);
    let registration = env.register();
    assert_eq!(registration.version, "0.5.6");
    assert!(
        registration.digest.starts_with("sha256:"),
        "{registration:?}"
    );
    let install = env.check(Some(&registration)).unwrap();
    assert_eq!(install.version, "0.5.6");
    assert_eq!(install.digest, registration.digest);
    assert!(!install.drifted);
}

// T45 (ADR 0033 §1, owner input 2026-10-22): SGLang 0.5.22 with LMCache
// 0.5.6 passes: the unified connector threads `cache_salt` end to end.
// The legacy SGLang connector files present in 0.5.6 are fine unused.
#[test]
fn lmcache_check_passes_on_sglang_0522_with_lmcache_056() {
    let env = Env::new(Engine::Sglang);
    env.write(
        "lmcache/integration/sglang/sglang_adapter.py",
        "class LMCacheConnector:\n    pass\n",
    );
    env.write(
        "lmcache/integration/sglang/multi_process_adapter.py",
        "class LMCacheMPConnector:\n    pass\n",
    );
    let install = env.check_registered().unwrap();
    assert_eq!(install.version, "0.5.6");
    assert!(!install.drifted);
}

/// A later 0.5.x release, reshaped where a release may reshape: quoted
/// differently, keys built in another method, other names around.
fn later_lmcache(env: &Env) {
    env.set_version("0.5.9");
    env.edit(
        "lmcache/v1/multiprocess/config.py",
        "\"--http-port\"",
        "'--http-port'",
    );
    env.edit(
        "lmcache/v1/distributed/config.py",
        "\"--eviction-policy\",",
        "'--eviction-policy',",
    );
    env.edit(
        "lmcache/v1/distributed/api.py",
        "    cache_salt = ipc_key.cache_salt\n",
        "    salt = ipc_key.cache_salt\n",
    );
    env.edit(
        "lmcache/v1/distributed/api.py",
        "cache_salt=cache_salt,",
        "cache_salt=salt,",
    );
}

// T45 (ADR 0033 §1): a later 0.5.x release whose text is reshaped still
// passes for both engines.
#[test]
fn lmcache_check_passes_on_a_later_05_release() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        let env = Env::new(engine);
        later_lmcache(&env);
        if engine == Engine::Sglang {
            env.edit(
                "lmcache/integration/sglang/unified_lmcache_mp_connector.py",
                "            cache_salt=operation.cache_salt,\n",
                "            cache_salt=(\n                operation.cache_salt\n            ),\n",
            );
        }
        let install = env.check_registered().unwrap();
        assert_eq!(install.version, "0.5.9");
    }
}

// T45 (ADR 0033 §§1–2): a later SGLang shape passes: its options declared in
// `server_args.py`, the registry branching with `elif` and importing the
// unified radix cache module whole, the salt passed as `cache_salt=`.
#[test]
fn lmcache_check_passes_on_a_later_sglang_shape() {
    let env = Env::new(Engine::Sglang);
    env.remove("sglang/srt/arg_groups/fields/memory.py");
    env.write(
        "sglang/srt/server_args.py",
        "@dataclasses.dataclass\nclass ServerArgs:\n    enable_lmcache: bool = False\n    \
         lmcache_config_file: Optional[str] = None\n",
    );
    env.edit(
        "sglang/srt/mem_cache/registry.py",
        "    if get_memory().enable_lmcache:\n        from sglang.srt.mem_cache.storage.lmcache.lmcache_unified_radix_cache import (\n            LMCacheUnifiedRadixCache,\n        )\n",
        "    if False:\n        pass\n    elif get_memory().enable_lmcache:\n        import sglang.srt.mem_cache.storage.lmcache.lmcache_unified_radix_cache as unified\n        LMCacheUnifiedRadixCache = unified.LMCacheUnifiedRadixCache\n",
    );
    env.edit(
        "sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py",
        "build_cache_salt(\n                    req.cache_salt, req.extra_key\n                )",
        "build_cache_salt(None, req.extra_key)",
    );
    env.check_registered().unwrap();
}

// T45 (ADR 0033 §1): each missing file fails `check_failed`, naming it.
#[test]
fn lmcache_check_names_each_missing_file() {
    let common = [
        "lmcache/v1/multiprocess/config.py",
        "lmcache/v1/distributed/config.py",
        "lmcache/cli/commands/server.py",
        "lmcache/cli/main.py",
        "lmcache/v1/multiprocess/custom_types.py",
        "lmcache/v1/distributed/api.py",
    ];
    let vllm = [
        "lmcache/integration/vllm/lmcache_mp_connector.py",
        "lmcache/integration/vllm/lmcache_mp_metadata.py",
        "vllm/config/kv_transfer.py",
    ];
    let sglang = [
        "lmcache/integration/sglang/unified_lmcache_mp_connector.py",
        "sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py",
        "sglang/srt/mem_cache/registry.py",
    ];
    for (engine, specific) in [(Engine::Vllm, &vllm), (Engine::Sglang, &sglang)] {
        for file in common.iter().chain(specific.iter()) {
            let env = Env::new(engine);
            env.remove(file);
            let detail = check_failed(&env);
            assert!(detail.contains(&format!("{file} is missing")), "{detail}");
        }
    }
    // The console script and its declaration.
    let env = Env::new(Engine::Vllm);
    std::fs::remove_file(env.root.join("bin/lmcache")).unwrap();
    assert!(check_failed(&env).contains("bin/lmcache (the console script) is missing"));
    let env = Env::new(Engine::Vllm);
    let dist = env.site.join("lmcache-0.5.6.dist-info/entry_points.txt");
    std::fs::write(
        &dist,
        "[console_scripts]\nlmcache_server = lmcache.v1.server.__main__:main\n",
    )
    .unwrap();
    assert!(check_failed(&env)
        .contains("lmcache-*.dist-info/entry_points.txt declares no lmcache console script"));
    // SGLang's options, in neither location.
    let env = Env::new(Engine::Sglang);
    env.remove("sglang/srt/arg_groups/fields/memory.py");
    let detail = check_failed(&env);
    for field in ["enable_lmcache", "lmcache_config_file"] {
        assert!(
            detail.contains(&format!("SGLang declares no {field} option")),
            "{detail}"
        );
    }
}

// T45 (ADR 0033 §3): each server option CapyCTL renders must be declared;
// each one missing is named.
#[test]
fn lmcache_check_names_each_missing_server_option() {
    let cases = [
        ("lmcache/v1/multiprocess/config.py", "\"--host\"", "--host"),
        ("lmcache/v1/multiprocess/config.py", "\"--port\"", "--port"),
        (
            "lmcache/v1/multiprocess/config.py",
            "\"--transport\"",
            "--transport",
        ),
        (
            "lmcache/v1/multiprocess/config.py",
            "\"--http-host\"",
            "--http-host",
        ),
        (
            "lmcache/v1/multiprocess/config.py",
            "\"--http-port\"",
            "--http-port",
        ),
        (
            "lmcache/v1/distributed/config.py",
            "\"--l1-size-gb\"",
            "--l1-size-gb",
        ),
        (
            "lmcache/v1/distributed/config.py",
            "\"--eviction-policy\"",
            "--eviction-policy",
        ),
    ];
    for (file, declared, option) in cases {
        let env = Env::new(Engine::Vllm);
        env.edit(file, declared, "\"--renamed\"");
        let detail = check_failed(&env);
        assert!(
            detail.contains(&format!("{file} declares no {option} option")),
            "{detail}"
        );
    }
    let env = Env::new(Engine::Vllm);
    env.edit(
        "lmcache/v1/distributed/config.py",
        "choices=[\"LRU\", ",
        "choices=[",
    );
    env.edit("lmcache/v1/distributed/config.py", "('LRU', ", "(");
    assert!(check_failed(&env).contains("declares no LRU eviction policy"));
    // `--no-l1-use-lazy` (eager L1, ADR 0033 §3, §6): a plain store-true
    // `--l1-use-lazy` has no negation and fails; an explicit
    // `--no-l1-use-lazy` option passes.
    let env = Env::new(Engine::Vllm);
    env.edit(
        "lmcache/v1/distributed/config.py",
        "action=argparse.BooleanOptionalAction,",
        "action=\"store_true\",",
    );
    assert!(check_failed(&env)
        .contains("lmcache/v1/distributed/config.py declares no --no-l1-use-lazy option"));
    env.edit(
        "lmcache/v1/distributed/config.py",
        "        \"--l1-use-lazy\",\n",
        "        \"--l1-use-lazy\",\n        \"--no-l1-use-lazy\",\n",
    );
    env.check_registered().unwrap();
    let env = Env::new(Engine::Vllm);
    env.edit(
        "lmcache/cli/commands/server.py",
        "            add_storage_manager_args(parser)\n",
        "",
    );
    env.edit(
        "lmcache/cli/commands/server.py",
        "from lmcache.v1.distributed.config import add_storage_manager_args\n",
        "\n",
    );
    assert!(check_failed(&env)
        .contains("lmcache/cli/commands/server.py does not compose add_storage_manager_args"));
}

// T45 (ADR 0033 §§1, 4): each missing class or field is named.
#[test]
fn lmcache_check_names_each_missing_class_and_field() {
    let env = Env::new(Engine::Vllm);
    env.edit(
        "lmcache/integration/vllm/lmcache_mp_connector.py",
        "class LMCacheMPConnector(",
        "class LMCacheConnectorV1(",
    );
    assert!(check_failed(&env).contains(
        "lmcache/integration/vllm/lmcache_mp_connector.py defines no class LMCacheMPConnector"
    ));
    let env = Env::new(Engine::Vllm);
    env.edit(
        "vllm/config/kv_transfer.py",
        "kv_connector_module_path:",
        "kv_connector_module:",
    );
    assert!(check_failed(&env).contains(
        "vllm/config/kv_transfer.py declares no kv_connector_module_path on KVTransferConfig"
    ));
    let env = Env::new(Engine::Sglang);
    env.edit(
        "lmcache/integration/sglang/unified_lmcache_mp_connector.py",
        "class UnifiedLMCacheMPConnector:",
        "class LMCacheMPConnector:",
    );
    assert!(check_failed(&env).contains("defines no class UnifiedLMCacheMPConnector"));
    for field in ["enable_lmcache", "lmcache_config_file"] {
        let env = Env::new(Engine::Sglang);
        env.edit(
            "sglang/srt/arg_groups/fields/memory.py",
            &format!("    {field}: A["),
            "    other: A[",
        );
        let detail = check_failed(&env);
        assert!(
            detail.contains(&format!("SGLang declares no {field} option")),
            "{detail}"
        );
    }
    let env = Env::new(Engine::Sglang);
    env.edit(
        "sglang/srt/mem_cache/registry.py",
        "if get_memory().enable_lmcache:",
        "if get_memory().enable_other_cache:",
    );
    assert!(check_failed(&env).contains(
        "sglang/srt/mem_cache/registry.py does not select lmcache_unified_radix_cache under enable_lmcache"
    ));
}

// T45 (ADR 0033 §5, owner input on LMCache issue 4825, 2026-10-22): SGLang
// is checked for salt support element by element, each named.
#[test]
fn lmcache_sglang_check_names_each_missing_salt_element() {
    let cases: [(&str, &str, &str, &str); 7] = [
        (
            "lmcache/integration/sglang/unified_lmcache_mp_connector.py",
            "            cache_salt=operation.cache_salt,\n",
            "",
            "unified_lmcache_mp_connector.py builds no IPCCacheServerKey with cache_salt=",
        ),
        (
            "lmcache/v1/multiprocess/custom_types.py",
            "    cache_salt: str = \"\"\n",
            "",
            "custom_types.py declares no cache_salt field on IPCCacheServerKey",
        ),
        (
            "lmcache/v1/distributed/api.py",
            "    cache_salt: str = \"\"\n",
            "",
            "api.py declares no cache_salt field on ObjectKey",
        ),
        (
            "lmcache/v1/distributed/api.py",
            "    cache_salt = ipc_key.cache_salt\n",
            "    cache_salt = \"\"\n",
            "api.py does not copy ipc_key.cache_salt into ObjectKey",
        ),
        (
            "lmcache/v1/distributed/api.py",
            "                cache_salt=cache_salt,\n",
            "",
            "api.py does not copy ipc_key.cache_salt into ObjectKey",
        ),
        (
            "sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py",
            "            cache_salt=req.cache_salt,\n",
            "",
            "does not pass req.cache_salt to LMCache",
        ),
        (
            "sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py",
            "from lmcache.integration.sglang.unified_lmcache_mp_connector import (",
            "from lmcache.integration.sglang.connector import (",
            "does not import lmcache.integration.sglang.unified_lmcache_mp_connector",
        ),
    ];
    for (file, from, to, named) in cases {
        let env = Env::new(Engine::Sglang);
        env.edit(file, from, to);
        if named.contains("req.cache_salt") {
            env.edit(
                file,
                "build_cache_salt(\n                    req.cache_salt, req.extra_key\n                )",
                "build_cache_salt(None, req.extra_key)",
            );
        }
        let detail = check_failed(&env);
        assert!(detail.contains(named), "{named}: {detail}");
        if !named.starts_with("does not import") {
            assert!(detail.contains("no cache_salt support"), "{detail}");
        }
    }
}

// T45 (ADR 0033 §5, owner input 2026-10-22): an SGLang whose LMCache radix
// cache uses the legacy connector (which drops the salt) fails, whichever
// legacy module and import form it uses, and so does a registry selecting
// the legacy radix cache.
#[test]
fn lmcache_sglang_legacy_path_fails() {
    const RADIX: &str = "sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py";
    for legacy_import in [
        "from lmcache.integration.sglang.sglang_adapter import (\n    LMCacheLayerwiseConnector,\n)\n",
        "from lmcache.integration.sglang import multi_process_adapter\n",
        "import lmcache.integration.sglang.sglang_adapter as adapter\n",
    ] {
        let env = Env::new(Engine::Sglang);
        env.edit(RADIX, "import torch\n", &format!("import torch\n{legacy_import}"));
        let detail = check_failed(&env);
        assert!(
            detail.contains("no cache_salt support")
                && detail.contains("imports the legacy lmcache.integration.sglang."),
            "{detail}"
        );
    }
    let env = Env::new(Engine::Sglang);
    env.edit(
        "sglang/srt/mem_cache/registry.py",
        "        from sglang.srt.mem_cache.storage.lmcache.lmcache_unified_radix_cache import (\n            LMCacheUnifiedRadixCache,\n        )\n",
        "        from sglang.srt.mem_cache.storage.lmcache.lmcache_radix_cache import (\n            LMCRadixCache as LMCacheUnifiedRadixCache,\n        )\n",
    );
    assert!(check_failed(&env).contains("does not select lmcache_unified_radix_cache"));
}

// T45 (ADR 0033 §1): LMCache below 0.5.6 (CVE-2026-105192) and another minor
// line fail `version_unsupported`, before anything else is read.
#[test]
fn lmcache_versions_outside_056_and_later_05_are_unsupported() {
    for version in ["0.5.5", "0.4.9", "0.6.0", "1.0.0"] {
        let env = Env::new(Engine::Vllm);
        let registration = env.register();
        env.set_version(version);
        let failure = env.check(Some(&registration)).unwrap_err();
        assert_eq!(
            failure.reason,
            LmcacheOffReason::VersionUnsupported,
            "{version}: {failure}"
        );
        assert!(failure.detail.contains(version), "{failure}");
    }
}

// T45 (ADR 0033 §1, LC6): an environment without LMCache is `not_installed`,
// and `engine add` records nothing for it.
#[test]
fn lmcache_absent_is_not_installed() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        let env = Env::new(engine);
        std::fs::remove_dir_all(env.site.join("lmcache")).unwrap();
        assert_eq!(
            measure_lmcache(&env.measurer, &env.executable()).unwrap(),
            None
        );
        let stale = LmcacheRegistration::new("0.5.6", "sha256:00");
        for registered in [None, Some(&stale)] {
            let failure = env.check(registered).unwrap_err();
            assert_eq!(failure.reason, LmcacheOffReason::NotInstalled, "{failure}");
        }
    }
}

// T45 (ADR 0033 §1): LMCache installed after `engine add` (the profile has
// no registration) is `not_registered`; it is not loaded.
#[test]
fn lmcache_installed_after_registration_is_not_registered() {
    let env = Env::new(Engine::Sglang);
    let failure = env.check(None).unwrap_err();
    assert_eq!(failure.reason, LmcacheOffReason::NotRegistered, "{failure}");
    assert!(
        failure.detail.contains("register the engine again"),
        "{failure}"
    );
}

// T45 T14 (ADR 0033 §1, ADR 0008): a changed LMCache file is drift, reported
// with the measured digest for the caller's `installation_drift` policy; a
// new bytecode cache is not.
#[test]
fn lmcache_digest_drift_is_reported() {
    let env = Env::new(Engine::Vllm);
    let registration = env.register();
    env.write(
        "lmcache/v1/__pycache__/api.cpython-312.pyc",
        "bytecode the interpreter rewrites",
    );
    let unchanged = env.check(Some(&registration)).unwrap();
    assert!(!unchanged.drifted);
    assert_eq!(unchanged.digest, registration.digest);
    env.write("lmcache/v1/patched.py", "PATCHED = True\n");
    let drifted = env.check(Some(&registration)).unwrap();
    assert!(drifted.drifted);
    assert_ne!(drifted.digest, registration.digest);
    assert!(drifted.digest.starts_with("sha256:"));
    assert_eq!(drifted.version, "0.5.6");
}

// T45 (ADR 0033 §1, SPEC §8.1): the check takes no process tool and reads
// files only: every program of the environment (its interpreter included)
// would record that it ran, on a pass and on every failure path.
#[test]
fn lmcache_check_executes_nothing() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        let env = Env::new(engine);
        env.check_registered().unwrap();
        env.check(None).unwrap_err();
        env.remove("lmcache/v1/distributed/api.py");
        env.check_registered().unwrap_err();
        env.set_version("0.6.0");
        env.check(None).unwrap_err();
        env.assert_nothing_ran();
    }
}

// T45 (ADR 0033 §2): the check is for vLLM and SGLang only.
#[test]
fn lmcache_check_refuses_engines_without_an_integration() {
    let env = Env::new(Engine::Vllm);
    let registration = env.register();
    for (engine, detail) in [
        (Engine::Llamacpp, "llama.cpp has no LMCache integration"),
        (Engine::Tensorfold, "TensorFold has no LMCache integration"),
        (
            Engine::Custom,
            "CapyCTL does not render options for a custom engine",
        ),
    ] {
        let failure = check_lmcache(
            &env.measurer,
            engine,
            &env.executable(),
            Some(&registration),
        )
        .unwrap_err();
        assert_eq!(failure.reason, LmcacheOffReason::EngineUnsupported);
        assert_eq!(failure.detail, detail);
    }
}

// T45 (ADR 0033 §1): an installation that is there but not one readable
// dist-info is recorded by nobody and fails the check, naming it.
#[test]
fn lmcache_without_one_dist_info_fails() {
    let env = Env::new(Engine::Vllm);
    let registration = env.register();
    std::fs::create_dir_all(env.site.join("lmcache-0.5.7.dist-info")).unwrap();
    assert!(measure_lmcache(&env.measurer, &env.executable()).is_err());
    let failure = env.check(Some(&registration)).unwrap_err();
    assert_eq!(failure.reason, LmcacheOffReason::CheckFailed);
    assert!(
        failure.detail.contains("no single lmcache-*.dist-info"),
        "{failure}"
    );
}

// T45 T21 T22 (ADR 0033 §1, ADR 0008): the host's launch-time check of a
// profile's LMCache, through the installation registry its launches use.
// Unchanged, it passes and the profile's status carries no LMCache drift.
// One installed LMCache file changed, the check measures another `sha256:`
// digest: the launch goes on under `warn`, is refused `installation_drift`
// under `refuse`, and the status the host reports carries the measured digest
// in `lmcache_observed_digest` until a check measures the registered digest
// again.
#[test]
fn a_drifted_lmcache_is_reported_in_the_profile_status() {
    use capyctl_agent::installation::{InstallationDrift, InstallationRegistry};
    use capyctl_protocol::pb;
    let env = Env::new(Engine::Vllm);
    let registration = env.register();
    let published = pb::RuntimeProfileStatus {
        name: "vllm-lmcache".into(),
        ..Default::default()
    };
    let registry = InstallationRegistry::from_inventory(&pb::ReportInventory {
        profiles: vec![published.clone()],
        ..Default::default()
    });
    let launch = |policy| {
        let verdict = registry.verify_lmcache(
            "vllm-lmcache",
            env.engine,
            &env.executable(),
            Some(&registration),
            policy,
        );
        env.assert_nothing_ran();
        verdict
    };
    let status = || {
        let mut profiles = [published.clone()];
        registry.overlay(&mut profiles);
        let [profile] = profiles;
        profile.lmcache_observed_digest
    };

    let unchanged = launch(InstallationDrift::Refuse).unwrap().unwrap();
    assert!(!unchanged.drifted);
    assert_eq!(status(), "", "an unchanged LMCache shows no drift");

    let file = "lmcache/v1/multiprocess/config.py";
    let original = std::fs::read_to_string(env.site.join(file)).unwrap();
    env.write(file, &format!("{original}\n# patched after engine add\n"));
    let drifted = launch(InstallationDrift::Warn).unwrap().unwrap();
    assert!(drifted.drifted, "warn uses LMCache and shows the drift");
    assert_ne!(drifted.digest, registration.digest);
    let observed = status();
    assert_eq!(observed, drifted.digest);
    // The form the server accepts (`sha256:` and 64 lowercase hex digits).
    let hex = observed.strip_prefix("sha256:").expect("a sha256 digest");
    assert_eq!(hex.len(), 64, "{observed}");
    assert!(
        hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{observed}"
    );
    assert_eq!(
        launch(InstallationDrift::Refuse).unwrap_err(),
        "installation_drift"
    );
    assert_eq!(status(), observed, "a refused launch still shows the drift");

    env.write(file, &original);
    assert!(!launch(InstallationDrift::Refuse).unwrap().unwrap().drifted);
    assert_eq!(
        status(),
        "",
        "measured as registered again, the drift clears"
    );
}
