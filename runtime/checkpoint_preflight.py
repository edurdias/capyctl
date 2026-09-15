"""Bounded observation of pinned checkpoint files, without engine effects."""

from contextlib import ExitStack
from dataclasses import dataclass, field
import errno
import hashlib
import json
import math
import os
import stat
import sys

from .checkpoint_manifest import _PINNED_MANIFEST


_CHUNK = 1024 * 1024
_ANCILLARY_LIMIT = 16 * _CHUNK
_U64_MAX = (1 << 64) - 1
_CODES = frozenset({
    "invalid_root", "unsupported_platform", "unsafe_file", "artifact_missing",
    "artifact_changed", "artifact_mismatch", "invalid_json", "invalid_storage",
    "unsupported_geometry", "io_error",
})


class CheckpointPreflightError(Exception):
    """A sanitized failure category."""

    def __init__(self, code):
        self.code = code if code in _CODES else "io_error"
        super().__init__(self.code)


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
    details: object


@dataclass(frozen=True)
class VerifiedCheckpoint:
    """Immutable scoped facts, never native launch or admission authority."""

    model_repository: str
    model_revision: str
    artifacts: tuple[VerifiedArtifact, ...]
    ancestors: tuple[DirectoryIdentity, ...]
    tensor_count: int
    payload_bytes: int
    model_maximum_context: int
    index_total_size: int
    index_discrepancy: int
    _root: str = field(repr=False)

    @property
    def root_identity(self):
        return self.ancestors[-1]


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
        raise CheckpointPreflightError("invalid_root")
    try:
        root.encode("utf-8")
    except UnicodeError:
        raise CheckpointPreflightError("invalid_root") from None


def _check_platform():
    if (sys.platform != "linux" or not all(hasattr(os, flag) for flag in
            ("O_DIRECTORY", "O_NOFOLLOW", "O_NONBLOCK", "O_CLOEXEC"))):
        raise CheckpointPreflightError("unsupported_platform")


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


def _read_artifact(fd, artifact, size):
    digest = hashlib.sha256()
    captured = bytearray()

    def consume(length, retain):
        while length:
            chunk = os.read(fd, min(_CHUNK, length))
            if not chunk:
                raise CheckpointPreflightError("artifact_changed")
            digest.update(chunk)
            if retain:
                captured.extend(chunk)
            length -= len(chunk)

    if artifact.name.endswith(".safetensors"):
        if size < 8:
            raise CheckpointPreflightError("invalid_storage")
        consume(8, True)
        header_size = int.from_bytes(captured, "little")
        if not 0 < header_size <= _CHUNK or header_size > size - 8:
            raise CheckpointPreflightError("invalid_storage")
        consume(header_size, True)
        consume(size - 8 - header_size, False)
    else:
        consume(size, artifact.name in ("config.json", "model.safetensors.index.json"))
    return digest.hexdigest(), bytes(captured)


def _observe(root, manifest, inspect=None):
    """Private fixture seam; production entrypoints never take a manifest."""
    try:
        _check_platform()
        _check_root(root)
        with ExitStack() as stack:
            chain = _open_chain(root, stack)
            ancestors = tuple(_directory_identity(fd) for fd in chain)
            records = []
            opened = []
            contents = {}
            for artifact in manifest.artifacts:
                fd = os.open(artifact.name, os.O_RDONLY | os.O_NOFOLLOW
                             | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=chain[-1])
                stack.callback(os.close, fd)
                before = os.fstat(fd)
                if not stat.S_ISREG(before.st_mode):
                    raise CheckpointPreflightError("unsafe_file")
                identity = _file_identity(before)
                if (not 0 <= before.st_size <= _U64_MAX
                        or (artifact.size is not None and before.st_size != artifact.size)
                        or (artifact.size is None and before.st_size > _ANCILLARY_LIMIT)):
                    raise CheckpointPreflightError("artifact_mismatch")
                digest, content = _read_artifact(fd, artifact, before.st_size)
                if _file_identity(os.fstat(fd)) != identity:
                    raise CheckpointPreflightError("artifact_changed")
                if digest != artifact.sha256:
                    raise CheckpointPreflightError("artifact_mismatch")
                records.append(VerifiedArtifact(artifact.name, digest, before.st_size, identity))
                opened.append(fd)
                contents[artifact.name] = content
            details = inspect(contents) if inspect is not None else None
            try:
                current = _open_chain(root, stack)
                if (tuple(_directory_identity(fd) for fd in current) != ancestors
                        or tuple(_directory_identity(fd) for fd in chain) != ancestors):
                    raise CheckpointPreflightError("artifact_changed")
                for record, fd in zip(records, opened):
                    entry = os.stat(record.name, dir_fd=current[-1], follow_symlinks=False)
                    if (not stat.S_ISREG(entry.st_mode)
                            or _file_identity(entry) != record.identity
                            or _file_identity(os.fstat(fd)) != record.identity):
                        raise CheckpointPreflightError("artifact_changed")
            except OSError:
                raise CheckpointPreflightError("artifact_changed") from None
            return _Observation(tuple(records), ancestors, details)
    except CheckpointPreflightError as error:
        raise CheckpointPreflightError(error.code) from None
    except OSError as error:
        raise CheckpointPreflightError(_error_from_os(error)) from None


def verify_checkpoint(root: str) -> VerifiedCheckpoint:
    """Verify the compiled-in pinned checkpoint, with no engine effects."""
    return _verify(root, _PINNED_MANIFEST)


def _expected_tensors(geometry):
    hidden = geometry.hidden
    intermediate = geometry.intermediate
    head = geometry.head_dim
    query = geometry.heads * head
    key_value = geometry.kv_heads * head
    result = {"model.embed_tokens.weight": (geometry.vocabulary, hidden),
              "model.norm.weight": (hidden,)}
    suffixes = {
        "input_layernorm.weight": (hidden,),
        "post_attention_layernorm.weight": (hidden,),
        "mlp.down_proj.weight": (hidden, intermediate),
        "mlp.gate_proj.weight": (intermediate, hidden),
        "mlp.up_proj.weight": (intermediate, hidden),
        "self_attn.k_norm.weight": (head,),
        "self_attn.q_norm.weight": (head,),
        "self_attn.k_proj.weight": (key_value, hidden),
        "self_attn.v_proj.weight": (key_value, hidden),
        "self_attn.q_proj.weight": (query, hidden),
        "self_attn.o_proj.weight": (hidden, query),
    }
    for layer in range(geometry.layers):
        for suffix, shape in suffixes.items():
            result[f"model.layers.{layer}.{suffix}"] = shape
    return result


def _u64(value):
    if type(value) is not int or not 0 <= value <= _U64_MAX:
        raise CheckpointPreflightError("invalid_storage")
    return value


def _tensor_bytes(shape):
    if not isinstance(shape, (list, tuple)) or not 1 <= len(shape) <= 8:
        raise CheckpointPreflightError("invalid_storage")
    size = 2  # The selected format has only BF16 tensors.
    for dimension in shape:
        dimension = _u64(dimension)
        if dimension == 0 or dimension > _U64_MAX // size:
            raise CheckpointPreflightError("invalid_storage")
        size *= dimension
    return size


def _strict_json(content):
    """Bound nesting before the decoder recurses, then reject ambiguous JSON."""
    def pairs(entries):
        result = {}
        for key, value in entries:
            if key in result:
                raise ValueError
            result[key] = value
        return result

    def integer(value):
        parsed = int(value)
        if not -_U64_MAX <= parsed <= _U64_MAX:
            raise ValueError
        return parsed

    def floating(value):
        parsed = float(value)
        if not math.isfinite(parsed):
            raise ValueError
        return parsed

    def constant(value):
        raise ValueError

    def check_strings(value):
        if isinstance(value, str):
            value.encode("utf-8")
        elif isinstance(value, dict):
            for key, child in value.items():
                key.encode("utf-8")
                check_strings(child)
        elif isinstance(value, list):
            for child in value:
                check_strings(child)

    try:
        text = content.decode("utf-8")
        depth = 0
        quoted = False
        escaped = False
        for character in text:
            if quoted:
                if escaped:
                    escaped = False
                elif character == "\\":
                    escaped = True
                elif character == '"':
                    quoted = False
            elif character == '"':
                quoted = True
            elif character in "[{":
                depth += 1
                if depth > 32:
                    raise ValueError
            elif character in "]}":
                depth -= 1
        value = json.loads(text, object_pairs_hook=pairs, parse_int=integer,
                           parse_float=floating, parse_constant=constant)
        check_strings(value)
        return value
    except (ValueError, UnicodeError, RecursionError):
        raise CheckpointPreflightError("invalid_json") from None


def _validate_config(config, geometry):
    # This full structure is from the exact pinned 727-byte config, not model defaults.
    expected = {
        "architectures": ["Qwen3ForCausalLM"], "attention_bias": False,
        "attention_dropout": 0.0, "bos_token_id": 151643, "eos_token_id": 151645,
        "head_dim": geometry.head_dim, "hidden_act": "silu", "hidden_size": geometry.hidden,
        "initializer_range": 0.02, "intermediate_size": geometry.intermediate,
        "max_position_embeddings": geometry.maximum_context, "max_window_layers": geometry.layers,
        "model_type": "qwen3", "num_attention_heads": geometry.heads,
        "num_hidden_layers": geometry.layers, "num_key_value_heads": geometry.kv_heads,
        "rms_norm_eps": 1e-6, "rope_scaling": None, "rope_theta": 5000000,
        "sliding_window": None, "tie_word_embeddings": True, "torch_dtype": "bfloat16",
        "transformers_version": "4.51.0", "use_cache": True, "use_sliding_window": False,
        "vocab_size": geometry.vocabulary,
    }
    if not isinstance(config, dict) or config.keys() != expected.keys():
        raise CheckpointPreflightError("unsupported_geometry")
    for key, value in expected.items():
        if type(config[key]) is not type(value) or config[key] != value:
            raise CheckpointPreflightError("unsupported_geometry")


def _tensor_name(name):
    if type(name) is not str or not 0 < len(name.encode("utf-8")) <= 256:
        raise CheckpointPreflightError("invalid_storage")


def _validate_storage(contents, manifest):
    _validate_config(_strict_json(contents["config.json"]), manifest.geometry)
    expected = _expected_tensors(manifest.geometry)
    index = _strict_json(contents["model.safetensors.index.json"])
    if not isinstance(index, dict) or set(index) != {"metadata", "weight_map"}:
        raise CheckpointPreflightError("invalid_storage")
    metadata, assignments = index["metadata"], index["weight_map"]
    if not isinstance(metadata, dict) or set(metadata) != {"total_size"}:
        raise CheckpointPreflightError("invalid_storage")
    advisory = _u64(metadata["total_size"])
    if not isinstance(assignments, dict) or len(assignments) > 4096:
        raise CheckpointPreflightError("invalid_storage")
    shards = {artifact.name: artifact.size for artifact in manifest.artifacts
              if artifact.name.endswith(".safetensors")}
    for name, shard in assignments.items():
        _tensor_name(name)
        if type(shard) is not str or shard not in shards:
            raise CheckpointPreflightError("invalid_storage")
    if assignments.keys() != expected.keys():
        raise CheckpointPreflightError("invalid_storage")
    seen = set()
    payload_total = 0
    for shard, size in shards.items():
        content = contents[shard]
        header = _strict_json(content[8:])
        if (not isinstance(header, dict)
                or len(header) - ("__metadata__" in header) > 4096):
            raise CheckpointPreflightError("invalid_storage")
        intervals = []
        for name, record in header.items():
            if name == "__metadata__":
                if (not isinstance(record, dict) or len(record) > 4096
                        or any(type(value) is not str for value in record.values())):
                    raise CheckpointPreflightError("invalid_storage")
                continue
            _tensor_name(name)
            if (name not in expected or name in seen or assignments[name] != shard
                    or not isinstance(record, dict)
                    or set(record) != {"dtype", "shape", "data_offsets"}):
                raise CheckpointPreflightError("invalid_storage")
            shape = record["shape"]
            tensor_size = _tensor_bytes(shape)
            offsets = record["data_offsets"]
            if (record["dtype"] != "BF16" or tuple(shape) != expected[name]
                    or not isinstance(offsets, list) or len(offsets) != 2):
                raise CheckpointPreflightError("invalid_storage")
            start, end = (_u64(offset) for offset in offsets)
            if end < start or end - start != tensor_size:
                raise CheckpointPreflightError("invalid_storage")
            intervals.append((start, end))
            seen.add(name)
        cursor = 0
        for start, end in sorted(intervals):
            if start != cursor:
                raise CheckpointPreflightError("invalid_storage")
            cursor = end
        if cursor != size - len(content) or payload_total > _U64_MAX - cursor:
            raise CheckpointPreflightError("invalid_storage")
        payload_total += cursor
    if seen != expected.keys() or advisory - payload_total != manifest.index_discrepancy:
        raise CheckpointPreflightError("invalid_storage")
    return len(seen), payload_total, advisory


def _verify(root, manifest):
    observation = _observe(root, manifest, lambda contents: _validate_storage(contents, manifest))
    count, payload, advisory = observation.details
    return VerifiedCheckpoint(
        "Qwen/Qwen3-4B-Instruct-2507", "cdbee75f17c01a7cc42f958dc650907174af0554",
        observation.artifacts, observation.ancestors, count, payload,
        manifest.geometry.maximum_context, advisory, advisory - payload, root,
    )


def _revalidate(previous, manifest):
    if type(previous) is not VerifiedCheckpoint:
        raise CheckpointPreflightError("invalid_root") from None
    current = _verify(previous._root, manifest)
    if current.ancestors != previous.ancestors or current.artifacts != previous.artifacts:
        raise CheckpointPreflightError("artifact_changed") from None
    return current


def revalidate_checkpoint(previous: VerifiedCheckpoint) -> VerifiedCheckpoint:
    """Repeat full verification and require the same root/file identities."""
    return _revalidate(previous, _PINNED_MANIFEST)
