//! ADR 0037 §7: the launch-time check of engine tracing, by shape.
//!
//! Like ADR 0033 §1's LMCache check, it reads the registered environment's
//! files and executes nothing (no interpreter, no import). It passes when the
//! engine's `site-packages` holds every distribution and module both engines
//! import when tracing starts:
//!
//! - vLLM 0.32.0 `vllm/tracing/otel.py` and SGLang 0.5.22
//!   `python/sglang/srt/observability/trace.py` import, in one `try` block,
//!   `opentelemetry.trace` (with `trace.propagation.tracecontext`),
//!   `opentelemetry.sdk.resources`, `opentelemetry.sdk.trace` and both
//!   `opentelemetry.exporter.otlp.proto.grpc.trace_exporter` and
//!   `opentelemetry.exporter.otlp.proto.http.trace_exporter`. Either exporter
//!   missing fails the whole import: vLLM's start then fails ("OpenTelemetry
//!   is not available"), SGLang's raises "opentelemetry package is not
//!   installed". So the gRPC exporter is required too, though CapyCTL only
//!   ever selects `http/protobuf`.
//! - The distributions are the `opentelemetry-api`, `opentelemetry-sdk`,
//!   `opentelemetry-proto`, `opentelemetry-exporter-otlp-proto-common`,
//!   `opentelemetry-exporter-otlp-proto-http` and
//!   `opentelemetry-exporter-otlp-proto-grpc` wheels (dist-info names with
//!   `_`), and the HTTP and gRPC exporters' transports, `requests` and
//!   `grpc` (`grpcio`).
//!
//! Missing anything: the launch runs without engine tracing
//! (`engine_tracing_off:not_installed`); the check never fails a start. A pass
//! is not qualification (AGENTS.md): OT5 is the live check.

use std::path::Path;

use capyctl_config::engine_policy::Engine;

use crate::installation::{dist_info, site_packages};

/// The distributions (dist-info prefixes) a traced engine imports.
pub const DISTRIBUTIONS: &[&str] = &[
    "opentelemetry_api",
    "opentelemetry_sdk",
    "opentelemetry_proto",
    "opentelemetry_exporter_otlp_proto_common",
    "opentelemetry_exporter_otlp_proto_http",
    "opentelemetry_exporter_otlp_proto_grpc",
    "requests",
    "grpcio",
];

/// The module files those imports load, relative to `site-packages`.
pub const MODULES: &[&str] = &[
    "opentelemetry/trace/__init__.py",
    "opentelemetry/trace/propagation/tracecontext.py",
    "opentelemetry/sdk/resources/__init__.py",
    "opentelemetry/sdk/trace/__init__.py",
    "opentelemetry/sdk/trace/export/__init__.py",
    "opentelemetry/exporter/otlp/proto/http/trace_exporter/__init__.py",
    "opentelemetry/exporter/otlp/proto/grpc/trace_exporter/__init__.py",
    "requests/__init__.py",
    "grpc/__init__.py",
];

/// ADR 0037 §7: what the environment of `executable` (an `engine` profile's)
/// lacks for engine tracing, by path relative to `site-packages` or by
/// distribution; empty when it has everything. An engine without a readable
/// `site-packages` lacks it all.
pub fn missing_tracing_packages(engine: Engine, executable: &Path) -> Vec<String> {
    let Ok(site) = site_packages(engine, executable) else {
        return vec![format!("the {} site-packages", engine.name())];
    };
    let mut missing: Vec<String> = DISTRIBUTIONS
        .iter()
        .filter(|name| dist_info(&site, name).is_none())
        .map(|name| format!("{name}-*.dist-info"))
        .collect();
    missing.extend(
        MODULES
            .iter()
            .filter(|module| {
                !std::fs::symlink_metadata(site.join(module)).is_ok_and(|m| m.is_file())
            })
            .map(|module| (*module).to_owned()),
    );
    missing
}

/// Whether the environment of `executable` can trace (nothing missing).
pub fn tracing_installed(engine: Engine, executable: &Path) -> bool {
    missing_tracing_packages(engine, executable).is_empty()
}

/// ADR 0037 §§7, 11: one launch's engine tracing, from this role's host
/// setting, the launch's engine and registered build, its retention (the
/// deployment's or the host's, `retention_zero`) and whether it is a group
/// member, reading the environment only when everything else would trace.
pub fn decide_launch(
    engine: Engine,
    engine_version: &str,
    executable: &Path,
    retention_zero: bool,
    group: bool,
) -> capyctl_adapters::engine_tracing::EngineTracing {
    use capyctl_adapters::engine_tracing::{decide, host_setting, EngineTracing, EngineTracingOff};
    let host = host_setting();
    // `not_installed` is the last reason (`decide`'s precedence), so the
    // environment is read only for a launch that would otherwise trace.
    match decide(engine, engine_version, &host, retention_zero, group, true) {
        EngineTracing::On { .. } if !tracing_installed(engine, executable) => {
            EngineTracing::Off(EngineTracingOff::NotInstalled)
        }
        decided => decided,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A venv tree: `<root>/bin/python3`, `<root>/bin/vllm`, and
    /// `<root>/lib/python3.12/site-packages/vllm/`.
    fn venv() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();
        std::fs::write(bin.join("vllm"), "#!/usr/bin/env python3\n").unwrap();
        let site = root.path().join("lib/python3.12/site-packages");
        std::fs::create_dir_all(site.join("vllm")).unwrap();
        (root, bin.join("vllm"), site)
    }

    fn install(site: &Path) {
        for name in DISTRIBUTIONS {
            std::fs::create_dir_all(site.join(format!("{name}-1.39.0.dist-info"))).unwrap();
        }
        for module in MODULES {
            let path = site.join(module);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }

    // T48 (ADR 0037 §7): the shape check finds every distribution and module
    // in a complete environment and names each one missing otherwise.
    #[test]
    fn the_shape_check_finds_and_misses_the_tracing_packages() {
        let (_root, executable, site) = venv();
        let missing = missing_tracing_packages(Engine::Vllm, &executable);
        assert_eq!(
            missing.len(),
            DISTRIBUTIONS.len() + MODULES.len(),
            "{missing:?}"
        );
        assert!(!tracing_installed(Engine::Vllm, &executable));
        install(&site);
        assert_eq!(
            missing_tracing_packages(Engine::Vllm, &executable),
            Vec::<String>::new()
        );
        assert!(tracing_installed(Engine::Vllm, &executable));
        // The HTTP exporter's module gone; then the gRPC exporter's wheel.
        std::fs::remove_file(
            site.join("opentelemetry/exporter/otlp/proto/http/trace_exporter/__init__.py"),
        )
        .unwrap();
        assert_eq!(
            missing_tracing_packages(Engine::Vllm, &executable),
            ["opentelemetry/exporter/otlp/proto/http/trace_exporter/__init__.py"]
        );
        install(&site);
        std::fs::remove_dir(site.join("opentelemetry_exporter_otlp_proto_grpc-1.39.0.dist-info"))
            .unwrap();
        assert_eq!(
            missing_tracing_packages(Engine::Vllm, &executable),
            ["opentelemetry_exporter_otlp_proto_grpc-*.dist-info"]
        );
        // An environment without the engine's own package lacks it all.
        assert!(!tracing_installed(Engine::Sglang, &executable));
    }
}
