"""Observe selected pinned SGLang sources without importing the native runtime.

This is not a complete package or binary attestation, effective argument mapping,
launch authorization, or memory-saver qualification. Same-service-user mutation
after verification remains outside the observation guarantee.
"""

from contextlib import ExitStack
from dataclasses import dataclass, field
import os
import stat

from .checkpoint_manifest import _Artifact, _Manifest
from .checkpoint_preflight import (
    CheckpointPreflightError, _check_platform, _check_root, _error_from_os,
    _observe, _open_chain,
)


_REVISION = "fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1"
_SOURCES = (
    ("server_args.py", "e04556de6d99ba8a76b91fffa49aa70ea9d65cd99f09d0da5d4cfe1228e28500"),
    ("utils/auth.py", "016734a0263cbc2bd6657481ab11535f1cac073efb7eed4bcdbf66f81dfd3ef7"),
    ("utils/torch_memory_saver_adapter.py", "196266b5ac6c7a805b36c9953fcdd9cafb0dcc3ac7a9e25aae6edf34a8c6fba9"),
    ("managers/scheduler.py", "159837693ec244d2dfb482d5788b8f4fa934c780505609c90a728939abe89815"),
    ("managers/scheduler_components/weight_updater.py", "60cc68d85a9399be91c681db68c3a57a4697354912483e3c7e61127caa7bc977"),
    ("entrypoints/engine.py", "3fc01012d5e06050767573a31ea6a5b07447e88136d23cb7fd8454743dac1db4"),
    ("entrypoints/http_server.py", "5cdce94aaf3a446ac43e5a8e46961cbafdca9de7f0234404d285ed47ef5274d6"),
    ("plugins/__init__.py", "3a975a73f1a7887e68c81ea7a2530250597ac8ae978efc0b0f70f038a99a3164"),
    ("platforms/__init__.py", "09592f9fc6d7c83625e40167b4b02d843decdac9359362782b440c43b80baca3"),
)
_CODES = frozenset(("invalid_root", "unsupported_platform", "unsafe_file",
                    "artifact_missing", "artifact_changed", "artifact_mismatch", "io_error"))


class SourcePreflightError(Exception):
    """Closed public category, with no source path or caught exception retained."""

    def __init__(self, code):
        self.code = code if code in _CODES else "io_error"
        super().__init__(self.code)


@dataclass(frozen=True)
class VerifiedSglangSources:
    source_revision: str
    directories: tuple
    _root: str = field(repr=False)


def _protected(info):
    if info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
        raise SourcePreflightError("unsafe_file")


def _permissions(parent, leaves):
    with ExitStack() as stack:
        chain = _open_chain(parent, stack)
        for fd in chain:
            _protected(os.fstat(fd))
        for leaf in leaves:
            fd = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                         dir_fd=chain[-1])
            stack.callback(os.close, fd)
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode):
                raise SourcePreflightError("unsafe_file")
            _protected(info)


def _observe_source_directories(root, inventory):
    """Shared protected source observation; no package identity is assigned here."""
    try:
        _check_platform()
        _check_root(root)
        groups = {}
        for relative, digest in inventory:
            parent, _, leaf = relative.rpartition("/")
            if not leaf or any(part in ("", ".", "..") for part in relative.split("/")):
                raise SourcePreflightError("invalid_root")
            groups.setdefault(parent, []).append(_Artifact(leaf, digest, None))
        records = []
        for relative, artifacts in sorted(groups.items()):
            parent = root + ("/" + relative if relative else "")
            leaves = tuple(artifact.name for artifact in artifacts)
            _permissions(parent, leaves)
            observation = _observe(parent, _Manifest(tuple(artifacts)))
            _permissions(parent, leaves)
            records.append((relative, observation))
        return tuple(records)
    except CheckpointPreflightError as error:
        raise SourcePreflightError(error.code) from None
    except OSError as error:
        raise SourcePreflightError(_error_from_os(error)) from None


def _observe_sources(root, inventory):
    """Private synthetic fixture seam; no public API accepts an inventory."""
    return VerifiedSglangSources(_REVISION, _observe_source_directories(root, inventory), root)


def verify_sglang_sources(root):
    """Observe the fixed nine-file inventory beneath an installed sglang/srt root."""
    return _observe_sources(root, _SOURCES)


def revalidate_sglang_sources(previous):
    """Repeat bytes and identities immediately before consuming source observations."""
    if type(previous) is not VerifiedSglangSources:
        raise SourcePreflightError("invalid_root")
    current = verify_sglang_sources(previous._root)
    if current != previous:
        raise SourcePreflightError("artifact_changed")
    return current
