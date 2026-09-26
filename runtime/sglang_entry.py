"""Protected SGLang startup boundary, using only the standard library.

The startup gates compose through sglang_native_composition before any engine
import: plugin closure and placement must hold as one native contract. ADR 0008
(owner decision 2026-09-23): no pinned source audit runs; after the guarded
import the internals this launch depends on are probed by shape
(engine_capabilities.py), and a missing one refuses only the dependent feature
with a closed `capability_missing:<name>` category. ADR 0014 §7, §9:
checkpoint identity is a content digest the host (remote agent or embedded
controller) measures against the recorded
digest before it starts this entry, and again before a wake; this entry
checks no pinned checkpoint manifest. Placement is asserted only when
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

import hmac
import json
import os
import stat
import sys

# The renderer invokes this file under python -IS. SPEC §9.1 / T21: resolve our
# own package from the verified runtime directory alone. The package `runtime`
# is registered with exactly this directory as its search path; its parent is
# never put on sys.path, where an unverified module could shadow a sibling or
# the standard library.
if __name__ in ("__main__", "__mp_main__") and not __package__:
    if "runtime" not in sys.modules:
        import types as _types
        _package = _types.ModuleType("runtime")
        _package.__path__ = [os.path.dirname(os.path.abspath(__file__))]
        _package.__package__ = "runtime"
        sys.modules["runtime"] = _package

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

_MAX_DESCRIPTOR = 65536
_MAX_EXTRA_ARGS = 256
_I64 = (1 << 63) - 1
_I32 = (1 << 31) - 1
_CODES = frozenset({"invalid_descriptor", "invalid_credentials", "descriptor_io",
                    "pinned_source_contract_unavailable", "memory_saver_unavailable",
                    "startup_error",
                    # The composition gates' own closed categories, surfaced
                    # verbatim through LaunchError when a gate refuses.
                    "invalid_launch_spec", "plugin_closure_failed",
                    "placement_failed",
                    # ADR 0008: a capability this launch depends on is missing
                    # from the installation (engine_capabilities.py).
                    "capability_missing:core", "capability_missing:deep_park",
                    # The argument mapper's closed categories (ADR 0014 §6).
                    "invalid_launch_inputs", "placement_mismatch",
                    "server_args_construction_failed", "effective_args_mismatch",
                    "invalid_extra_args", "memory_grant_unavailable",
                    # ADR 0014 §8 / SPEC §8.2: a sensitive destination the host
                    # did not approve, or a malformed approvals document.
                    "sensitive_option_refused", "invalid_extra_approvals",
                    # SPEC §8.2 / T21: the single-rank rendezvous stays off the
                    # network (loopback_rendezvous.py).
                    "loopback_rendezvous_failed"})
# ADR 0014 §2: the typed settings a deployment may state. None means the
# engine's own default applies; mllm validates type and range only (ADR 0011).
_SETTINGS = ("dtype", "quantization", "kv_cache_dtype", "context_length",
             "max_running_requests", "cuda_graphs", "language_model_only",
             "trust_remote_code", "max_total_tokens", "chunked_prefill_size",
             "tokenizer_workers", "memory_saver", "cpu_weight_backup",
             "weight_restore", "memory", "extra_args")


class LaunchError(Exception):
    """Closed, public failure category; never retains input values."""

    def __init__(self, code):
        self.code = code if code in _CODES else "startup_error"
        super().__init__(self.code)


from runtime.sglang_launch_spec import LaunchSpec


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


def _optional_token(value, limit=64):
    if value is None:
        return
    _text(value, limit)
    if not value.isascii() or any(not (char.isalnum() or char in "_-.") for char in value):
        _reject()


def _optional_integer(value, low, high):
    if value is not None:
        _integer(value, low, high)


def _boolean(value):
    if type(value) is not bool:
        _reject()


def _validate_settings(settings):
    """ADR 0014 §2, §5, §6: the closed typed settings object.

    Values pass through as the engine spells them; this checks type, range and
    closure only. Reserved settings never appear here: construct_server_args
    renders them from the binding, placement and grant.
    """
    _exact_object(settings, _SETTINGS)
    for name in ("dtype", "quantization", "kv_cache_dtype"):
        _optional_token(settings[name])
    if settings["dtype"] not in (None, "auto", "bfloat16", "float16", "float32"):
        _reject()
    for name in ("context_length", "max_running_requests", "max_total_tokens"):
        _optional_integer(settings[name], 1, _I32)
    chunked = settings["chunked_prefill_size"]
    if chunked is not None:
        _integer(chunked, -1, _I32)
        if chunked == 0:
            _reject()
    if settings["cuda_graphs"] is not None:
        _boolean(settings["cuda_graphs"])
    for name in ("language_model_only", "trust_remote_code", "memory_saver",
                 "cpu_weight_backup"):
        _boolean(settings[name])
    _integer(settings["tokenizer_workers"], 1, 1024)
    expected_restore = "cpu_backup" if settings["cpu_weight_backup"] else "disk_reload"
    _literal(settings["weight_restore"], expected_restore)
    memory = settings["memory"]
    keys = ("request_bytes", "kv_cache_bytes", "margin_bytes", "static_bytes")
    # Discrete GPU design §6: the card's total is stated only on a discrete
    # device, where the static pool is a share of the card, not MemAvailable.
    discrete = type(memory) is dict and "device_total_bytes" in memory
    _exact_object(memory, keys + ("device_total_bytes",) if discrete else keys)
    for name in ("request_bytes", "kv_cache_bytes", "static_bytes"):
        _integer(memory[name], 1, _I64)
    _integer(memory["margin_bytes"], 0, _I64)
    request, kv = memory["request_bytes"], memory["kv_cache_bytes"]
    if kv > request:
        _reject()
    if discrete:
        _integer(memory["device_total_bytes"], 1, 2**62 - 1)
        # Design §3: a device request is weights x 1.10 plus the KV cache, so
        # the margin is a tenth of the weights share.
        if memory["margin_bytes"] != (request - kv) // 11:
            _reject()
    # ADR 0014 §5: the static pool is the request minus the overhead margin,
    # floored at the declared KV cache and never above the whole request (an
    # explicit request below KV plus margin cannot honour the margin).
    if memory["static_bytes"] != min(max(request - memory["margin_bytes"], kv), request):
        _reject()
    extra = settings["extra_args"]
    if type(extra) is not list or len(extra) > _MAX_EXTRA_ARGS:
        _reject()
    for token in extra:
        _text(token, 4096)


def _validate_public(value):
    # ADR 0008 (owner decision 2026-09-23): no source revision token. The
    # installed SGLang build is identified by the host's installation
    # fingerprint and probed by shape (engine_capabilities.py), never pinned.
    _exact_object(value, ("schema_version", "kind", "engine",
                         "binding_id", "incarnation", "endpoint", "served_name",
                         "rendered_settings_digest", "settings", "device"))
    for key, expected in (("schema_version", 2), ("kind", "sglang_launch"),
                          ("engine", "sglang")):
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
    # validators refuse exactly the same inputs.
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
    _validate_settings(value["settings"])


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
    """Compose the engine package root from the launcher's own selection.

    The rendered command (args.rs render_for_launcher) pins the engine
    interpreter as argv[0]; under -IS no site processing selects a package
    root, so the path is composed explicitly from that executable's
    environment prefix. Nothing is discovered from PATH, PYTHONPATH, or .pth
    hooks. A wrong composition fails closed at the guarded import.
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


def _verified_native_contract(spec):
    """Run the startup gates and hold the resulting native contract.

    The gates run in composition's fixed order — plugin closure, placement —
    and any failure leaves through the gate's own closed category, never the
    retired blanket denial. The installation's own package metadata is made
    visible to the plugin closure from the launcher-selected interpreter's
    tree. When the descriptor carries the
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
    # SPEC §9.2: enumerate the selected installation's plugin metadata too.
    # -IS omits it; no .pth processing or native imports are performed here.
    search = os.path.dirname(os.path.dirname(_trusted_package_root()))
    if search not in sys.path:
        sys.path.append(search)
    try:
        if spec._placement_digest is None:
            return composition.compose(spec, trusted_mapping=None, placement_digest=None)
        try:
            # The mapping is the placement gate's own input, so an assembly
            # failure (for example a descriptor shape this boundary does not
            # accept) folds into the gate's closed placement_failed category,
            # never the blanket startup_error.
            mapping = _placement_mapping(spec, spec._placement_digest)
        except Exception:
            raise composition.NativeCompositionError("placement_failed") from None
        return composition.compose(spec, trusted_mapping=mapping,
                                   placement_digest=spec._placement_digest)
    except composition.NativeCompositionError as error:
        raise LaunchError(error.code) from None


def _guarded_engine_import():
    """The single guarded native import seam; only the held contract reaches it.

    Composes the trusted search path explicitly — never site.main(), never
    .pth hooks — and imports the engine startup modules inside this
    function only; every engine import in this process happens here. The
    search path is appended, so the stdlib and this protected package keep
    precedence over the verified tree. Tests substitute this seam; a failed
    import leaves through main's closed startup category.
    """
    search = os.path.dirname(os.path.dirname(_trusted_package_root()))
    if search not in sys.path:
        sys.path.append(search)
    from runtime.sglang_startup_guards import preimport_guard
    preimport_guard()
    from sglang.srt.entrypoints import http_server as launch
    from sglang.srt import server_args as arguments
    return arguments, launch


def _require_capabilities(spec, arguments, launch):
    """ADR 0008: probe, by shape, the internals this launch depends on.

    `core` (the ServerArgs record, resolution and fields the mapper renders and
    rechecks, and launch_server) is needed by every launch; `deep_park` (the
    memory saver hooks and the release, resume, reload and flush routes) only
    by a launch with the memory saver on, which is what `deep` residency
    renders. A `restart_only` launch therefore serves on a build without the
    saver hooks. Never file hashes, never permission bits.
    """
    from runtime import engine_capabilities, sglang_server_args
    settings = json.loads(spec._public_json)["settings"]
    reserved = (tuple(sglang_server_args._RESERVED_CONSTANT)
                + tuple(sglang_server_args._RESERVED_BOUND)
                + ("trust_remote_code", "tokenizer_worker_num"))
    try:
        engine_capabilities.require_sglang(settings, arguments, launch,
                                           reserved_fields=reserved)
    except engine_capabilities.CapabilityMissing as missing:
        raise LaunchError(missing.code) from None


def _import_and_launch(spec, contract, approvals=None):
    """The guarded launch boundary; the verified contract is the only key.

    The contract's placement is consumed exactly as composed: an unasserted
    digest is carried as placement=None and the audited argument mapper fails
    closed on it, never guessing a device. The scheduler observation bridge
    and protected listener attach to an initialized Scheduler inside the
    engine's own spawned scheduler process, which this parent cannot reach:
    SPEC §9.2, a memory-saver launch whose host supplied a private observation
    directory hands SGLang the enrolling scheduler target
    (sglang_observation_enrollment), which enrolls the observation there. No
    permissive fallback exists: an import or startup failure leaves through
    main's closed categories.
    """
    from runtime import loopback_rendezvous, sglang_server_args
    # SPEC §8.2 / T21: pinned before any engine import, so no engine module
    # reads a TCP rendezvous (torch's TCPStore binds every interface).
    try:
        rendezvous = loopback_rendezvous.pin("sglang")
    except loopback_rendezvous.RendezvousError:
        raise LaunchError("loopback_rendezvous_failed") from None
    arguments, launch = _guarded_engine_import()
    _require_capabilities(spec, arguments, launch)
    try:
        checked = sglang_server_args.construct_server_args(
            spec, contract.placement, arguments.ServerArgs, approvals=approvals)
    except sglang_server_args.ServerArgsError as error:
        # ADR 0014 §6: the mapper's closed category reaches the operator.
        raise LaunchError(error.code) from None
    if json.loads(spec._public_json)["settings"]["memory_saver"] is True:
        # Only a memory-saver launch is ever woken by a disk reload.
        _keep_served_name_on_reload()
    target = _observation_target(spec, launch)
    try:
        # Rechecked last: the scheduler inherits exactly what was verified.
        loopback_rendezvous.verify(rendezvous)
    except loopback_rendezvous.RendezvousError:
        raise LaunchError("loopback_rendezvous_failed") from None
    if target is None:
        launch.launch_server(checked._native)
    else:
        launch.launch_server(checked._native, run_scheduler_process_func=target)


def _keep_served_name_on_reload():
    """SPEC §3: the served name is mllm's (the route name), and stays so.

    Found live 2026-09-23 (M28, SGLang 0.5.20): `update_weights_from_disk`, the
    reload every deep wake performs, renames the served model to the checkpoint
    path (`TokenizerManager._update_model_path_info`). /v1/models then no longer
    listed the route name, the wake's fresh probe failed and the launch was
    left uncertain. The model path still updates; only the served name is kept.
    The tokenizer manager runs in this process, so the class is changed here,
    before launch. Returns whether this installation has the rename to undo.
    """
    module = sys.modules.get("sglang.srt.managers.tokenizer_manager")
    if module is None:
        try:
            from sglang.srt.managers import tokenizer_manager as module
        except ImportError:
            # No such manager in this installation: nothing renames there.
            return False
    manager = getattr(module, "TokenizerManager", None)
    original = getattr(manager, "_update_model_path_info", None)
    if manager is None or not callable(original):
        return False

    def update_model_path_info(self, *args, **kwargs):
        served = self.served_model_name
        result = original(self, *args, **kwargs)
        self.served_model_name = served
        return result

    manager._update_model_path_info = update_model_path_info
    return True


def _observation_target(spec, launch):
    """The enrolling scheduler target, or None when this launch enrolls nothing.

    The child scope carries the launch's declared weight restore, so the saver
    observer admits the weights backup of a `host_backed` launch only (ADR 0019).
    Only a memory-saver launch enrolls, only when the host supplied its private
    observation directory (MLLM_OBSERVATION_DIR) and it validates, and only when
    the installation's `launch_server` takes a scheduler target (the
    `observation` capability, engine_capabilities.py). Anything else serves
    without an observation, and the host refuses Park unchanged (fail closed).
    """
    from runtime import engine_capabilities, sglang_observation_enrollment as enrollment
    # Children see only a scope this entry validated, never the raw input.
    directory = os.environ.get(enrollment.ENV_DIR)
    enrollment.clear_environment()
    public = json.loads(spec._public_json)
    if (directory is None or public["settings"]["memory_saver"] is not True
            or not engine_capabilities.accepts_scheduler_target(launch)
            or not enrollment.entry_environment(directory, public["binding_id"],
                                                public["incarnation"],
                                                public["settings"]["weight_restore"])):
        return None
    return enrollment.run_enrolled_scheduler


def main(argv=None, descriptor_reader=None, stderr=None):
    """Sanitized startup status; no credential or checkpoint exception rendering."""
    stderr = sys.stderr if stderr is None else stderr
    try:
        # ADR 0014 §8: the host's approvals for sensitive extra arguments, read
        # and removed before anything else so no engine child inherits them.
        from runtime import extra_args_policy
        try:
            approvals = extra_args_policy.approvals_from_environment()
        except extra_args_policy.Refused:
            raise LaunchError("invalid_extra_approvals") from None
        spec = build_launch(sys.argv[1:] if argv is None else argv,
                            _read_descriptor if descriptor_reader is None else descriptor_reader)
        contract = _verified_native_contract(spec)
        _import_and_launch(spec, contract, approvals)
        return 0
    except LaunchError as error:
        code = error.code
    except Exception:
        code = "startup_error"
    stderr.write("sglang_startup_failed: " + code + _hint(code) + "\n")
    return 1


def _hint(code):
    """One fixed operator hint per capability category; never native detail."""
    if code == "capability_missing:deep_park":
        return (" (this SGLang installation lacks the memory saver hooks or routes"
                " deep parking needs; declare residency restart_only, or use a build"
                " that provides them)")
    if code == "capability_missing:core":
        return " (this SGLang installation lacks an interface every launch needs)"
    return ""


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
