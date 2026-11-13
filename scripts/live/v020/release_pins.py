#!/usr/bin/env python3
"""Download checksum-pinned vLLM release-candidate wheels using only stdlib.

Provenance: https://github.com/vllm-project/vllm/commit/
52aeb580658571613f8ab28a075a06fa30279beb. Both complete wheels were downloaded
and SHA256-hashed on 2026-11-12; their embedded _version.py identifies g52aeb5806.
Their METADATA declares Python >=3.11,<3.15, torch==2.14.0,
torchaudio==2.11.0, torchvision==0.29.0, flashinfer-python==0.7.3, and CUDA-13
extras on nvidia-cutlass-dsl and humming-kernels. Select torch's cu130 backend
when installing; downloading this wheel does not install its dependencies.

The owner approved a test-only LMCache 0.5.7 dependency override on 2026-11-12.
Its API <=1.40.0 and Prometheus exporter <=0.61b0 bounds conflict with vLLM's
API >=1.45.1. Keep the vLLM requirement and use the matching OTel 1.45.1 /
Prometheus exporter 0.66b1 family, with uv --override (never --no-deps).
Sources: https://github.com/LMCache/LMCache/blob/v0.5.7/requirements/common.txt
and https://pypi.org/pypi/opentelemetry-exporter-prometheus/0.66b1/json
(requires opentelemetry-sdk~=1.45.1). This exception is not qualification.
"""
import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


VLLM_VERSION = "0.34.0rc1"
VLLM_COMMIT = "52aeb580658571613f8ab28a075a06fa30279beb"
VLLM_SHA256 = {
    "x86_64": "3c816985a80f66cd333788ed107a25bd296e1ac3b81b601d4b9423eab567a042",
    "aarch64": "8227604f93957842acf163f71b15baf7396edd391ef00a00cbb83f9bc9264b3b",
}
CHUNK_SIZE = 1024 * 1024
LMCACHE_034_OVERRIDES = {
    "opentelemetry-api": "1.45.1",
    "opentelemetry-sdk": "1.45.1",
    "opentelemetry-exporter-otlp": "1.45.1",
    "opentelemetry-exporter-prometheus": "0.66b1",
}
LMCACHE_034_OVERRIDE_REASON = (
    "Owner-approved test-only exception (2026-11-12): LMCache 0.5.7's "
    "opentelemetry-api<=1.40.0 and prometheus exporter<=0.61b0 bounds "
    "conflict with vLLM 0.34.0rc1's opentelemetry-api>=1.45.1. "
    "Keep vLLM's requirement with OTel 1.45.1 and its matching exporter 0.66b1."
)


def lmcache_034_provenance(packages: dict, source: dict) -> dict:
    """Record the exception without implying native qualification."""
    installed = {name.lower().replace("_", "-"): version
                 for name, version in packages.items()}
    if source.get("version") != "0.5.7" or installed.get("lmcache") != "0.5.7":
        raise ValueError("the owner-approved override requires LMCache 0.5.7")
    if installed.get("vllm") != VLLM_VERSION:
        raise ValueError("the owner-approved override requires vLLM 0.34.0rc1")
    for name, version in LMCACHE_034_OVERRIDES.items():
        if installed.get(name) != version:
            raise ValueError(f"LMCache override version mismatch: {name}")
    return {
        "test_only": True, "qualified": False,
        "reason": LMCACHE_034_OVERRIDE_REASON,
        "resolver": "uv --override", "build_isolation": False,
        "build_torch": installed["torch"], "source": source,
        "overrides": LMCACHE_034_OVERRIDES, "packages": installed,
        "exporter_compatibility": "0.66b1 requires opentelemetry-sdk~=1.45.1",
        "vllm_commit": VLLM_COMMIT,
    }


def verify_lmcache_startup(environment: Path, evidence: Path, key: str) -> None:
    """Bounded, model-free MP smoke; stdout must pass the setup sanitizer."""
    environment, evidence = Path(environment), Path(evidence)
    result_path = evidence / f"{key}.startup.json"
    metrics_path = evidence / f"{key}.startup.metrics.txt"
    metrics_path.unlink(missing_ok=True)
    result = {"import_lmcache": False, "mp_startup": False, "metrics": False,
              "qualified": False, "startup_timeout_seconds": 90,
              "pool_gib": 0.125, "transfer_mode": "engine_driven",
              "row_metrics_verified": False,
              "required_row_counter_families": [
                  "lookup_hit_tokens", "lookup_requested_tokens", "num_chunks_loaded"],
              "row_metrics_reason": (
                  "LMCache 0.5.7 lookup.py records hit/requested only on "
                  "MP_LOOKUP_PREFETCH_END; engine.py records loaded only on "
                  "MP_RETRIEVE_END with retrieved_count>0. Rows must check "
                  "hit/requested after lookup and loaded after positive retrieval.")}
    process = None
    # No inherited collector, coordinator, plugins, or usage reporting in smoke.
    env = {name: value for name, value in os.environ.items()
           if not name.startswith(("LMCACHE_", "OTEL_", "PROMETHEUS_"))}
    env.update(LMCACHE_TRACK_USAGE="false", DO_NOT_TRACK="1")
    started = time.monotonic()
    try:
        subprocess.run([str(environment / "bin/python"), "-c", "import lmcache"],
                       check=True, timeout=30, env=env)
        result["import_lmcache"] = True
        with socket.socket() as rpc, socket.socket() as http:
            rpc.bind(("127.0.0.1", 0))
            http.bind(("127.0.0.1", 0))
            rpc_port, http_port = rpc.getsockname()[1], http.getsockname()[1]
        command = [
            str(environment / "bin/lmcache"), "server",
            "--host", "127.0.0.1", "--port", str(rpc_port), "--transport", "zmq",
            "--http-host", "127.0.0.1", "--http-port", str(http_port),
            "--l1-size-gb", "0.125", "--no-l1-use-lazy",
            "--eviction-policy", "LRU", "--eviction-trigger-watermark", "0.8",
            "--eviction-ratio", "0.2", "--supported-transfer-mode", "engine_driven",
        ]
        process = subprocess.Popen(command, env=env, start_new_session=True)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("LMCache MP startup process exited")
            try:
                base = f"http://127.0.0.1:{http_port}"
                with opener.open(base + "/healthcheck", timeout=2) as response:
                    if response.status != 200:
                        raise ValueError("LMCache healthcheck failed")
                with opener.open(base + "/metrics", timeout=2) as response:
                    body = response.read(4 * 1024 * 1024 + 1)
                    if response.status != 200 or len(body) > 4 * 1024 * 1024:
                        raise ValueError("invalid LMCache metrics response")
                text = body.decode()
                names = sorted(set(re.findall(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)"
                                               r"(?:\{[^\n]*\})?\s+[-+0-9.eE]+",
                                               text, re.MULTILINE)))
                # Native observable counter from EventBusSelfMetricsSubscriber:
                # unlike request counters, it reports real bus state before traffic.
                # Source: LMCache v0.5.7 subscribers/metrics/event_bus.py.
                native_names = [name for name in names if re.fullmatch(
                    r"lmcache_mp_event_bus_dropped_events(?:_total)?", name)]
                if not native_names:
                    raise ValueError("LMCache native EventBus metrics are absent")
                if process.poll() is not None:
                    raise RuntimeError("LMCache MP process exited during metrics scrape")
                metrics_path.write_text(text)
                result.update(mp_startup=True, metrics=True, metric_names=names,
                              native_metric_names=native_names)
                break
            except (OSError, ValueError, urllib.error.URLError):
                time.sleep(0.25)
        else:
            raise TimeoutError("LMCache MP startup/metrics exceeded 90 seconds")
    finally:
        try:
            if process is not None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait(timeout=5)
                result["process_stopped"] = process.returncode is not None
        finally:
            result["elapsed_seconds"] = time.monotonic() - started
            result_path.write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")


@dataclass(frozen=True)
class WheelPin:
    url: str
    sha256: str

    @property
    def filename(self) -> str:
        return self.url.rsplit("/", 1)[-1]


def vllm_wheel(arch: str) -> WheelPin:
    """Select the actual release artifact, not an index's mutable candidate."""
    arch = arch.lower()
    arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(arch, arch)
    if arch not in VLLM_SHA256:
        raise ValueError(f"unsupported architecture: {arch}")
    filename = f"vllm-{VLLM_VERSION}-cp38-abi3-manylinux_2_28_{arch}.whl"
    return WheelPin(f"https://wheels.vllm.ai/{VLLM_COMMIT}/{filename}",
                    VLLM_SHA256[arch])


def _check_sha256(path: Path, expected: str) -> None:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(CHUNK_SIZE), b""):
            digest.update(chunk)
    actual = digest.hexdigest()
    if actual != expected:
        raise ValueError(f"SHA256 mismatch for {path}: expected {expected}, got {actual}")


def download_wheel(pin: WheelPin, destination: Path) -> Path:
    """Return a verified wheel beneath destination; never publish partial bytes.

    An existing wheel is checked on every invocation. Corrupt cached files fail
    closed, leaving the rejected bytes available for operator investigation.
    """
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    target = destination / pin.filename
    if target.exists():
        _check_sha256(target, pin.sha256)
        return target
    partial = None
    try:
        with tempfile.NamedTemporaryFile(dir=destination, prefix=f".{pin.filename}.",
                                         suffix=".part", delete=False) as output:
            partial = Path(output.name)
            with urllib.request.urlopen(pin.url, timeout=120) as source:
                for chunk in iter(lambda: source.read(CHUNK_SIZE), b""):
                    output.write(chunk)
        _check_sha256(partial, pin.sha256)
        partial.replace(target)
    finally:
        if partial is not None:
            partial.unlink(missing_ok=True)
    return target


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", default=platform.machine(),
                        help="x86_64/amd64 or aarch64/arm64 (default: host architecture)")
    parser.add_argument("--destination", required=True, type=Path,
                        help="directory in which to cache the verified wheel")
    args = parser.parse_args()
    try:
        wheel = download_wheel(vllm_wheel(args.arch), args.destination)
    except (OSError, ValueError, urllib.error.URLError) as error:
        parser.exit(1, f"release_pins: {error}\n")
    print(wheel)


if __name__ == "__main__":
    main()
