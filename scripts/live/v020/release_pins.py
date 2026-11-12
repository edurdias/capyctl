#!/usr/bin/env python3
"""Download checksum-pinned vLLM release-candidate wheels using only stdlib.

Provenance: https://github.com/vllm-project/vllm/commit/
52aeb580658571613f8ab28a075a06fa30279beb. Both complete wheels were downloaded
and SHA256-hashed on 2026-11-12; their embedded _version.py identifies g52aeb5806.
Their METADATA declares Python >=3.11,<3.15, torch==2.14.0,
torchaudio==2.11.0, torchvision==0.29.0, flashinfer-python==0.7.3, and CUDA-13
extras on nvidia-cutlass-dsl and humming-kernels. Select torch's cu130 backend
when installing; downloading this wheel does not install its dependencies.

Compatibility warning: LMCache v0.5.7's requirements/common.txt caps
opentelemetry-api at 1.40.0, while these wheels require >=1.45.1. Its documented
no-build-isolation recipe supports building against installed torch 2.14.0 and
CUDA 13, but does NOT resolve that runtime dependency conflict. Do not bypass
the resolver with --no-deps or modify existing engine environments.
Sources: https://github.com/LMCache/LMCache/blob/v0.5.7/pyproject.toml
and https://github.com/LMCache/LMCache/blob/v0.5.7/requirements/common.txt.
"""
import argparse
from dataclasses import dataclass
import hashlib
from pathlib import Path
import platform
import tempfile
import urllib.error
import urllib.request


VLLM_VERSION = "0.34.0rc1"
VLLM_COMMIT = "52aeb580658571613f8ab28a075a06fa30279beb"
VLLM_SHA256 = {
    "x86_64": "3c816985a80f66cd333788ed107a25bd296e1ac3b81b601d4b9423eab567a042",
    "aarch64": "8227604f93957842acf163f71b15baf7396edd391ef00a00cbb83f9bc9264b3b",
}
CHUNK_SIZE = 1024 * 1024
LMCACHE_034_CONFLICT = (
    "vLLM 0.34.0rc1 + LMCache unavailable: vLLM requires "
    "opentelemetry-api>=1.45.1; LMCache 0.5.7 and current dev require "
    "opentelemetry-api<=1.40.0 and opentelemetry-exporter-prometheus<=0.61b0"
)


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
