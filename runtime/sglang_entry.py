"""Protected SGLang startup boundary, using only the standard library.

The audited startup gates compose through sglang_native_composition before any
engine import: pinned source revalidation, plugin closure, and checkpoint
revalidation must hold as one native contract. Placement is asserted only when
the descriptor carries the service-authorized device inventory digest the host
policy published; the mapping is then assembled from the descriptor's
service-frozen device selectors, that digest, and the inherited CUDA namespace,
and corroborated against freshly collected inventory. Without the digest the
launch boundary fails closed on that carried fact rather than inferring it as
satisfaction. The engine import
happens only inside the guarded boundary after the contract is held, through
the launcher-pinned package root and without site processing. A descriptor
validates shape and binds private inputs; it does not authorize a launch or
qualify memory release. The controller retains those obligations.
"""

from dataclasses import dataclass, field
import hmac
import json
import os
import stat
import sys

# The renderer invokes this file under python -IS. Resolve our own package from
# the installed wrapper location, never the current directory or PYTHONPATH.
if __name__ in ("__main__", "__mp_main__") and not __package__:
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

# CPython spawn prepares this main script before unpickling the Process object.
# Native argument classes can import SGLang during that unpickle, before any
# scheduler/detokenizer target runs. A target-function guard alone is too late.
# The production launcher must retain this protected script as the main path;
# alternative spawn/forkserver/main-module paths are not covered by this guard.
if __name__ == "__mp_main__":
    try:
        from runtime.sglang_startup_guards import preimport_guard
        preimport_guard()
    except BaseException:
        # No native/input exception text, including if containment itself fails.
        raise SystemExit(1) from None

from runtime.checkpoint_preflight import (
    CheckpointPreflightError, verify_checkpoint, revalidate_checkpoint,
)


_MAX_DESCRIPTOR = 65536
_RECIPE = "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1"
_SOURCE = "fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1"
_CHECKPOINT = "cdbee75f17c01a7cc42f958dc650907174af0554"
_MINIMUM_KV = 603979776
_CODES = frozenset({"invalid_descriptor", "invalid_credentials", "descriptor_io",
                    "pinned_source_contract_unavailable", "memory_saver_unavailable",
                    "startup_error",
                    # The composition gates' own closed categories, surfaced
                    # verbatim through LaunchError when a gate refuses.
                    "source_revalidation_failed", "plugin_closure_failed",
                    "placement_failed", "checkpoint_revalidation_failed"})
_SETTINGS = {
    "recipe": _RECIPE, "tensor_parallel_size": 1, "data_parallel_size": 1,
    "tokenizer_workers": 1, "model_dtype": "bfloat16", "context_tokens": 4096,
    "max_running_requests": 8, "max_total_tokens": 4096,
    "prefill_cuda_graphs": False, "decode_cuda_graphs": False,
    "memory_saver": True, "cpu_weight_backup": False, "speculative_decoding": False,
    "lora": False, "trust_remote_code": False, "disaggregation": False,
    "external_cache": False, "cpu_kv_offload": False, "native_grpc": False,
    "weight_restore": "disk_reload",
}


class LaunchError(Exception):
    """Closed, public failure category; never retains input values."""

    def __init__(self, code):
        self.code = code if code in _CODES else "startup_error"
        super().__init__(self.code)


@dataclass(frozen=True, repr=False)
class LaunchSpec:
    """Private process-local inputs, never a serialized or public DTO."""

    _public_json: str = field(repr=False)
    _checkpoint_root: str = field(repr=False)
    _inference_key: str = field(repr=False)
    _admin_key: str = field(repr=False)
    # Version 1 has no scope and must never be promoted implicitly. Version 2
    # carries immutable private metadata, not enrollment or execution authority,
    # plus the optional service-authorized device inventory digest the host
    # policy published; the entry never invents either.
    _launch_scope_json: str | None = field(default=None, repr=False)
    _placement_digest: str | None = field(default=None, repr=False)

    def __repr__(self):
        return "LaunchSpec(<private>)"


def _reject():
    raise LaunchError("invalid_descriptor") from None


def _text(value, limit):
    if type(value) is not str:
        _reject()
    try:
        if not 0 < len(value.encode("utf-8")) <= limit:
            _reject()
    except UnicodeError:
        _reject()
    if any(ord(char) < 32 or ord(char) == 127 for char in value):
        _reject()
    return value


def _strict_json(raw):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                _reject()
            result[key] = value
        return result

    def constant(value):
        _reject()

    try:
        if type(raw) is bytes:
            if not 0 < len(raw) <= _MAX_DESCRIPTOR:
                _reject()
            raw = raw.decode("utf-8")
        if type(raw) is not str or not 0 < len(raw.encode("utf-8")) <= _MAX_DESCRIPTOR:
            _reject()
        result = json.loads(raw, object_pairs_hook=pairs, parse_constant=constant)
        if type(result) is not dict:
            _reject()
        return result
    except (ValueError, TypeError, UnicodeError, RecursionError):
        _reject()


def _exact_object(value, keys):
    if type(value) is not dict or set(value) != set(keys):
        _reject()


def _literal(value, expected):
    if type(value) is not type(expected) or value != expected:
        _reject()


def _integer(value, low, high):
    if type(value) is not int or not low <= value <= high:
        _reject()


def _ulid(value):
    _text(value, 26)
    if (len(value) != 26 or value[0] not in "01234567"
            or any(char not in "0123456789ABCDEFGHJKMNPQRSTVWXYZ" for char in value)):
        _reject()


def _validate_public(value):
    _exact_object(value, ("schema_version", "kind", "engine", "recipe", "source_revision",
                         "checkpoint_revision", "binding_id", "incarnation", "endpoint",
                         "served_name", "rendered_settings_digest", "settings",
                         "minimum_kv_bytes", "static_memory_fraction", "device"))
    for key, expected in (("schema_version", 1), ("kind", "sglang_launch"),
                          ("engine", "sglang"), ("recipe", _RECIPE),
                          ("source_revision", _SOURCE), ("checkpoint_revision", _CHECKPOINT),
                          ("minimum_kv_bytes", _MINIMUM_KV)):
        _literal(value[key], expected)
    _ulid(value["binding_id"])
    _ulid(value["incarnation"])
    device = value["device"]
    _exact_object(device, ("host_id", "hardware_fingerprint", "device_id", "memory_domain"))
    for selector in device.values():
        _text(selector, 256)
        if not selector.isascii() or any(not (char.isalnum() or char in "_-.:")
                                         for char in selector):
            _reject()
    # The served name is the deployment's route (Spec §3), not a derived
    # binding artifact: an ASCII printable token, a non-empty run of at most
    # 256 bytes restricted to 0x21..=0x7E. This mirrors the coordinator's
    # `served_name_token` in crates/mllm-adapters/src/sglang/args.rs, so both
    # validators refuse exactly the same inputs. The retired
    # `candidate-{binding_id}` derivation is gone rather than deprecated.
    served = _text(value["served_name"], 256)
    if (not served.isascii() or not served.isprintable()
            or any(char.isspace() for char in served)):
        _reject()
    endpoint = _text(value["endpoint"], 128)
    prefix = "http://127.0.0.1:"
    port = endpoint[len(prefix):]
    if (not endpoint.startswith(prefix) or not port.isascii() or not port.isdigit()
            or not 1 <= int(port) <= 65535 or str(int(port)) != port):
        _reject()
    digest = _text(value["rendered_settings_digest"], 64)
    if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
        _reject()
    settings = value["settings"]
    _exact_object(settings, (*_SETTINGS, "requested_budget"))
    for key, expected in _SETTINGS.items():
        _literal(settings[key], expected)
    budget = settings["requested_budget"]
    _exact_object(budget, ("kv_cache_bytes", "static_memory_fraction_bps"))
    _integer(budget["kv_cache_bytes"], _MINIMUM_KV, (1 << 63) - 1)
    fraction = budget["static_memory_fraction_bps"]
    _integer(fraction, 1, 10000)
    _literal(value["static_memory_fraction"], f"{fraction // 10000}.{fraction % 10000:04d}")


def _credential(raw):
    if (type(raw) is not bytes or not 0 < len(raw) <= 4096
            or any(byte < 33 or byte > 126 for byte in raw)):
        raise LaunchError("invalid_credentials") from None
    return raw.decode("ascii")


def _validate_launch_scope(value, public):
    _exact_object(value, ("session_id", "deployment_id", "operation_id", "step_id",
                          "binding_id", "incarnation", "revision", "generation",
                          "issued_at_ms", "deadline_ms"))
    for name in ("session_id", "deployment_id", "operation_id", "step_id",
                 "binding_id", "incarnation"):
        _ulid(value[name])
    for name in ("revision", "generation"):
        _integer(value[name], 1, (1 << 63) - 1)
    for name in ("issued_at_ms", "deadline_ms"):
        _integer(value[name], 0, (1 << 63) - 1)
    if (value["deadline_ms"] <= value["issued_at_ms"]
            or value["binding_id"] != public["binding_id"]
            or value["incarnation"] != public["incarnation"]):
        _reject()
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def _read_descriptor(fd):
    """Consume a bounded inherited regular file; never open a caller-supplied path.

    The launcher must supply distinct private regular files (including memfds),
    positioned at zero. Pipes and sockets are rejected to avoid blocking reads.
    Ownership and mode checks are additional guards, not authority evidence.
    """
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or not 0 < info.st_size <= _MAX_DESCRIPTOR
                or os.lseek(fd, 0, os.SEEK_CUR) != 0):
            raise LaunchError("descriptor_io")
        result = bytearray()
        while len(result) <= _MAX_DESCRIPTOR:
            chunk = os.read(fd, min(4096, _MAX_DESCRIPTOR + 1 - len(result)))
            if not chunk:
                break
            result.extend(chunk)
        if len(result) != info.st_size:
            raise LaunchError("descriptor_io")
        return bytes(result)
    except OSError:
        raise LaunchError("descriptor_io") from None
    finally:
        try:
            os.close(fd)
        except OSError:
            pass


def build_launch(argv, descriptor_reader):
    """Validate the closed argv and protected inputs, with no engine imports."""
    try:
        names = ("--public-settings-json", "--launch-descriptor-fd",
                 "--inference-credential-fd", "--admin-credential-fd")
        if type(argv) not in (list, tuple) or len(argv) != 8:
            _reject()
        options = {}
        for name, value in zip(argv[::2], argv[1::2]):
            if type(name) is not str or name not in names or name in options:
                _reject()
            options[name] = value
        fds = []
        for name in names[1:]:
            raw = _text(options[name], 10)
            if not raw.isascii() or not raw.isdigit():
                _reject()
            fd = int(raw)
            if not 3 <= fd <= (1 << 31) - 1 or raw != str(fd) or fd in fds:
                _reject()
            fds.append(fd)
        public = _strict_json(options[names[0]])
        _validate_public(public)
        private = _strict_json(descriptor_reader(fds[0]))
        version = private.get("schema_version")
        _integer(version, 1, 2)
        keys = ("schema_version", "kind", "checkpoint_root", "public_settings")
        if version == 1:
            _exact_object(private, keys)
            digest = None
        else:
            scope_keys = (*keys, "launch_scope")
            # The private descriptor carries the service-authorized device
            # inventory digest (the host policy's published claim, frozen into
            # the launch) as one optional field. Its presence is the entry's
            # only instruction to assert placement; its value must be the
            # exact 64 lowercase hex digits the collector computes.
            if set(private) == set((*scope_keys, "placement_digest")):
                digest = private["placement_digest"]
                _text(digest, 64)
                if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
                    _reject()
            else:
                _exact_object(private, scope_keys)
                digest = None
        _literal(private["kind"], "sglang_private_launch")
        _validate_public(private["public_settings"])
        if private["public_settings"] != public:
            _reject()
        launch_scope = (None if version == 1
                        else _validate_launch_scope(private["launch_scope"], public))
        root = _text(private["checkpoint_root"], 4096)
        if not root.startswith("/") or any(part in ("", ".", "..") for part in root[1:].split("/")):
            _reject()
        inference = _credential(descriptor_reader(fds[1]))
        admin = _credential(descriptor_reader(fds[2]))
        if hmac.compare_digest(inference, admin):
            raise LaunchError("invalid_credentials")
        return LaunchSpec(json.dumps(public, sort_keys=True, separators=(",", ":")),
                          root, inference, admin, launch_scope, digest)
    except LaunchError as error:
        raise LaunchError(error.code) from None
    except Exception:
        raise LaunchError("invalid_descriptor") from None


def _trusted_package_root():
    """Compose the pinned engine package root from the launcher's own selection.

    The rendered command (args.rs render_for_launcher) pins the engine
    interpreter as argv[0]; under -IS no site processing selects a package
    root, so the path is composed explicitly from that executable's
    environment prefix. Nothing is discovered from PATH, PYTHONPATH, or .pth
    hooks. A wrong composition fails closed in source revalidation with that
    gate's own category.
    """
    prefix = os.path.dirname(os.path.dirname(sys.executable))
    return os.path.join(prefix, "lib", "python%d.%d" % sys.version_info[:2],
                        "site-packages", "sglang", "srt")


def _placement_mapping(spec, digest):
    """Assemble the service-supplied placement inputs for the digest, or None.

    The descriptor's device selectors (host/hardware/device/memory) are
    service-frozen values delivered over the protected descriptor fd, and the
    digest is the host policy's published inventory claim frozen into the same
    descriptor. The physical UUID is the one complete verified UUID in the
    inherited CUDA namespace — the guarded-service obligation sglang_device
    documents — observed here, never invented. observe_placement corroborates
    the assembled mapping against freshly collected inventory; nothing here is
    placement evidence by construction.
    """
    from runtime.sglang_device import TrustedDeviceMapping
    device = json.loads(spec._public_json)["device"]
    return TrustedDeviceMapping(
        host_id=device["host_id"], hardware_fingerprint=device["hardware_fingerprint"],
        device_id=device["device_id"], memory_domain=device["memory_domain"],
        physical_gpu_uuid=os.environ.get("CUDA_VISIBLE_DEVICES"),
        inventory_digest=digest)


def _verified_native_contract(spec, checkpoint):
    """Run the audited startup gates and hold the resulting native contract.

    The gates run in composition's fixed order — pinned source revalidation,
    plugin closure, placement, checkpoint revalidation — and any failure
    leaves through the gate's own closed category, never the retired blanket
    denial. The entry supplies the package root composed from the
    launcher-selected interpreter. When the descriptor carries the
    service-authorized inventory digest, the entry passes it with a
    TrustedDeviceMapping assembled from the descriptor's service-frozen device
    selectors, that digest, and the inherited CUDA namespace, so placement is
    asserted against freshly collected inventory; otherwise compose is invoked
    with placement_digest=None and the contract records
    placement_asserted=False as an explicit unmet obligation, never as
    placement evidence. A digest the entry cannot honestly pair with a mapping
    input (for example no inherited namespace) fails closed through the
    placement gate's own category.
    """
    from runtime import sglang_native_composition as composition
    try:
        if spec._placement_digest is None:
            return composition.compose(spec, checkpoint,
                                       package_root=_trusted_package_root(),
                                       trusted_mapping=None, placement_digest=None)
        try:
            # The mapping is the placement gate's own input, so an assembly
            # failure (for example a descriptor shape this boundary does not
            # accept) folds into the gate's closed placement_failed category,
            # never the blanket startup_error.
            mapping = _placement_mapping(spec, spec._placement_digest)
        except Exception:
            raise composition.NativeCompositionError("placement_failed") from None
        return composition.compose(spec, checkpoint,
                                   package_root=_trusted_package_root(),
                                   trusted_mapping=mapping,
                                   placement_digest=spec._placement_digest)
    except composition.NativeCompositionError as error:
        raise LaunchError(error.code) from None


def _guarded_engine_import():
    """The single guarded native import seam; only the held contract reaches it.

    Composes the trusted search path explicitly — never site.main(), never
    .pth hooks — and imports the pinned engine startup modules inside this
    function only; every engine import in this process happens here. The
    search path is appended, so the stdlib and this protected package keep
    precedence over the verified tree. Tests substitute this seam; a failed
    import leaves through main's closed startup category.
    """
    search = os.path.dirname(_trusted_package_root())
    if search not in sys.path:
        sys.path.append(search)
    import sglang.launch_server as launch
    from sglang.srt import server_args as arguments
    return arguments, launch


def _import_and_launch(spec, checkpoint, contract):
    """The guarded launch boundary; the verified contract is the only key.

    The contract's placement is consumed exactly as composed: an unasserted
    digest is carried as placement=None and the audited argument mapper fails
    closed on it, never guessing a device. The scheduler observation bridge
    and protected listener are engine-side wiring — they attach to an
    initialized Scheduler inside the engine's own spawned interpreter, which
    this parent process cannot reach — so no observation attachment happens
    here and that enrollment remains required before any live launch. No
    permissive fallback exists: an import or startup failure leaves through
    main's closed categories.
    """
    from runtime import sglang_server_args
    arguments, launch = _guarded_engine_import()
    checked = sglang_server_args.construct_server_args(
        spec, contract.placement, arguments.ServerArgs)
    launch.launch_server(checked._native)


def main(argv=None, descriptor_reader=None, stderr=None):
    """Sanitized startup status; no credential or checkpoint exception rendering."""
    stderr = sys.stderr if stderr is None else stderr
    try:
        spec = build_launch(sys.argv[1:] if argv is None else argv,
                            _read_descriptor if descriptor_reader is None else descriptor_reader)
        checkpoint = verify_checkpoint(spec._checkpoint_root)
        contract = _verified_native_contract(spec, checkpoint)
        checkpoint = revalidate_checkpoint(checkpoint)
        _import_and_launch(spec, checkpoint, contract)
        return 0
    except (LaunchError, CheckpointPreflightError) as error:
        code = error.code
    except Exception:
        code = "startup_error"
    stderr.write("sglang_startup_failed: " + code + "\n")
    return 1


def _normalized_path(path):
    _text(path, 8192)
    if not path.startswith("/") or "#" in path or "\\" in path:
        _reject()
    path = path.split("?", 1)[0]
    for _ in range(4):
        if "%" not in path:
            break
        decoded = bytearray()
        cursor = 0
        while cursor < len(path):
            if path[cursor] == "%":
                hex_value = path[cursor + 1:cursor + 3]
                if len(hex_value) != 2 or any(char not in "0123456789abcdefABCDEF" for char in hex_value):
                    _reject()
                decoded.append(int(hex_value, 16))
                cursor += 3
            else:
                decoded.extend(path[cursor].encode("utf-8"))
                cursor += 1
        path = decoded.decode("utf-8")
    _text(path, 8192)
    if any(char in path for char in ("%", "\\", "?", "#")):
        _reject()
    parts = []
    for part in path.split("/"):
        if part == "..":
            if parts:
                parts.pop()
        elif part not in ("", "."):
            parts.append(part)
    return "/" + "/".join(parts)


def private_health_allowed(path, authorization, inference_key):
    """Additional health gate only; native inference/admin auth still applies."""
    try:
        normalized = _normalized_path(path)
        if not normalized.startswith("/health"):
            return True
        key = _credential(inference_key.encode("ascii"))
        if type(authorization) is not str or len(authorization) > 4103:
            return False
        return hmac.compare_digest(authorization.encode("ascii"), ("Bearer " + key).encode("ascii"))
    except (LaunchError, UnicodeError, AttributeError, TypeError):
        return False


class PrivateHealthMiddleware:
    """ASGI health authorization without body reads, dispatch, or control proxies."""

    def __init__(self, app, inference_key):
        self._app = app
        self._inference_key = _credential(inference_key.encode("ascii"))

    async def __call__(self, scope, receive, send):
        if scope.get("type") == "http":
            headers = [value for name, value in scope.get("headers", ())
                       if name.lower() == b"authorization"]
            try:
                authorization = headers[0].decode("ascii") if len(headers) == 1 else ""
            except UnicodeError:
                authorization = ""
            paths = [scope.get("path")]
            if "raw_path" in scope:
                try:
                    paths.append(scope["raw_path"].decode("ascii"))
                except (UnicodeError, AttributeError):
                    paths.append(None)
            if not all(private_health_allowed(path, authorization, self._inference_key) for path in paths):
                await send({"type": "http.response.start", "status": 401,
                            "headers": [(b"content-type", b"text/plain"),
                                        (b"content-length", b"12")]})
                await send({"type": "http.response.body", "body": b"Unauthorized"})
                return
        await self._app(scope, receive, send)


if __name__ == "__main__":
    raise SystemExit(main())
