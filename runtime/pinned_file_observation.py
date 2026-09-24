"""Descriptor-safe observation of named files against pinned SHA-256 values.

Re-homed from the retired checkpoint preflight (ADR 0014 §9): checkpoint
identity is now a digest the host agent measures in Rust (ADR 0014 §7). ADR
0008 (owner decision 2026-09-23) retired the pinned SGLang and saver source
inventories as well; only the saver library binding still uses this open
chain. The pinned-manifest reader has no caller left, and no path compares
engine installation files to hard-coded hashes.

Nothing here imports an engine, loads a model or authorizes a launch.
"""

from contextlib import ExitStack
from dataclasses import dataclass
import errno
import hashlib
import os
import stat
import sys


_CHUNK = 1024 * 1024
_ANCILLARY_LIMIT = 16 * _CHUNK
_U64_MAX = (1 << 64) - 1
_CODES = frozenset({
    "invalid_root", "unsupported_platform", "unsafe_file", "artifact_missing",
    "artifact_changed", "artifact_mismatch", "io_error",
})


class PinnedFileError(Exception):
    """A sanitized failure category."""

    def __init__(self, code):
        self.code = code if code in _CODES else "io_error"
        super().__init__(self.code)


@dataclass(frozen=True)
class _Artifact:
    name: str
    sha256: str
    size: int | None


@dataclass(frozen=True)
class _Manifest:
    artifacts: tuple[_Artifact, ...]


@dataclass(frozen=True)
class DirectoryIdentity:
    device: int
    inode: int


@dataclass(frozen=True)
class FileIdentity:
    device: int
    inode: int
    size: int
    mtime_ns: int
    ctime_ns: int


@dataclass(frozen=True)
class VerifiedArtifact:
    name: str
    sha256: str
    size: int
    identity: FileIdentity


@dataclass(frozen=True)
class _Observation:
    artifacts: tuple[VerifiedArtifact, ...]
    ancestors: tuple[DirectoryIdentity, ...]


def _file_identity(value):
    return FileIdentity(value.st_dev, value.st_ino, value.st_size,
                        value.st_mtime_ns, value.st_ctime_ns)


def _directory_identity(fd):
    value = os.fstat(fd)
    return DirectoryIdentity(value.st_dev, value.st_ino)


def _error_from_os(error):
    if error.errno == errno.ENOENT:
        return "artifact_missing"
    if error.errno in (errno.ELOOP, errno.ENOTDIR, errno.ENXIO):
        return "unsafe_file"
    return "io_error"


def _check_root(root):
    if (type(root) is not str or not root.startswith("/") or "\0" in root
            or (root != "/" and any(part in ("", ".", "..") for part in root[1:].split("/")))):
        raise PinnedFileError("invalid_root")
    try:
        root.encode("utf-8")
    except UnicodeError:
        raise PinnedFileError("invalid_root") from None


def _check_platform():
    if (sys.platform != "linux" or not all(hasattr(os, flag) for flag in
            ("O_DIRECTORY", "O_NOFOLLOW", "O_NONBLOCK", "O_CLOEXEC"))):
        raise PinnedFileError("unsupported_platform")


def _open_chain(root, stack):
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open("/", flags)
    stack.callback(os.close, fd)
    chain = [fd]
    for part in (() if root == "/" else root[1:].split("/")):
        fd = os.open(part, flags, dir_fd=fd)
        stack.callback(os.close, fd)
        chain.append(fd)
    return chain


def _read_artifact(fd, size):
    digest = hashlib.sha256()
    length = size
    while length:
        chunk = os.read(fd, min(_CHUNK, length))
        if not chunk:
            raise PinnedFileError("artifact_changed")
        digest.update(chunk)
        length -= len(chunk)
    return digest.hexdigest()


def _observe(root, manifest):
    """Hash each named file under `root` and require its pinned value and a
    stable identity from open to the end of the observation."""
    try:
        _check_platform()
        _check_root(root)
        with ExitStack() as stack:
            chain = _open_chain(root, stack)
            ancestors = tuple(_directory_identity(fd) for fd in chain)
            records = []
            opened = []
            for artifact in manifest.artifacts:
                fd = os.open(artifact.name, os.O_RDONLY | os.O_NOFOLLOW
                             | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=chain[-1])
                stack.callback(os.close, fd)
                before = os.fstat(fd)
                if not stat.S_ISREG(before.st_mode):
                    raise PinnedFileError("unsafe_file")
                identity = _file_identity(before)
                if (not 0 <= before.st_size <= _U64_MAX
                        or (artifact.size is not None and before.st_size != artifact.size)
                        or (artifact.size is None and before.st_size > _ANCILLARY_LIMIT)):
                    raise PinnedFileError("artifact_mismatch")
                digest = _read_artifact(fd, before.st_size)
                if _file_identity(os.fstat(fd)) != identity:
                    raise PinnedFileError("artifact_changed")
                if digest != artifact.sha256:
                    raise PinnedFileError("artifact_mismatch")
                records.append(VerifiedArtifact(artifact.name, digest, before.st_size, identity))
                opened.append(fd)
            try:
                current = _open_chain(root, stack)
                if (tuple(_directory_identity(fd) for fd in current) != ancestors
                        or tuple(_directory_identity(fd) for fd in chain) != ancestors):
                    raise PinnedFileError("artifact_changed")
                for record, fd in zip(records, opened):
                    entry = os.stat(record.name, dir_fd=current[-1], follow_symlinks=False)
                    if (not stat.S_ISREG(entry.st_mode)
                            or _file_identity(entry) != record.identity
                            or _file_identity(os.fstat(fd)) != record.identity):
                        raise PinnedFileError("artifact_changed")
            except OSError:
                raise PinnedFileError("artifact_changed") from None
            return _Observation(tuple(records), ancestors)
    except PinnedFileError as error:
        raise PinnedFileError(error.code) from None
    except OSError as error:
        raise PinnedFileError(_error_from_os(error)) from None
