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
import ipaddress
import json
import os
import signal
import stat
import sys
import threading
import time

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
        # No native/input exception text, including if the guard itself fails.
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
                    "loopback_rendezvous_failed",
                    # SPEC §§6.4, 13.2: SGLang's launch raised after it started
                    # engine processes (a scheduler that exited before readiness).
                    "engine_launch_failed",
                    # ADR 0028 §10: a group field SGLang's resolution changed.
                    "group_drift:tp_size", "group_drift:pp_size", "group_drift:nnodes",
                    "group_drift:node_rank", "group_drift:dist_init_addr"})
# ADR 0014 §2: the typed settings a deployment may state. None means the
# engine's own default applies; capyctl validates type and range only (ADR 0011).
_SETTINGS = ("dtype", "quantization", "kv_cache_dtype", "context_length",
             "max_running_requests", "cuda_graphs", "language_model_only",
             "trust_remote_code", "max_total_tokens", "chunked_prefill_size",
             "tokenizer_workers", "memory_saver", "cpu_weight_backup",
             "weight_restore", "memory", "extra_args")
# ADR 0024: the parsers capyctl chose at launch. Present only when chosen, so
# every earlier launch descriptor stays exactly as it was.
_PARSER_SETTINGS = ("tool_call_parser", "reasoning_parser")
# ADR 0014 amendment A14: the recurrent-state slots capyctl sized for a hybrid
# model. Present only when sized, so every other launch descriptor is unchanged.
# ADR 0028 §10: `group` is present only on a multi-node group member (T39).
# ADR 0008 amendment 2026-10-08: `draft_model_path`, the declared drafter's
# directory capyctl materialized, is present only with a drafter.
# ADR 0033 §4: `lmcache_config_file`, the launch's owner-only LMCache
# connector file, is present only on a launch whose LMCache server is up; it
# turns on the reserved `enable_lmcache` (sglang_server_args.py).
# ADR 0037 §7: `trace_endpoint`, the host's loopback OTLP/HTTP endpoint, is
# present only on a traced single launch; it turns on the reserved
# `enable_trace`, `otlp_traces_endpoint` and `otlp_service_name`.
_OPTIONAL_SETTINGS = _PARSER_SETTINGS + ("max_mamba_cache_size", "group", "draft_model_path",
                                         "lmcache_config_file", "trace_endpoint")
# ADR 0028 §10: a member's group object (crates/capyctl-adapters/src/sglang/
# args.rs `public_group`); `gloo_socket_ifname` only when the host resolved one.
_GROUP_KEYS = ("tp_size", "pp_size", "nnodes", "node_rank", "dist_init_addr", "host_ip")
GROUP_IFNAME_KEY = "gloo_socket_ifname"
# The protected entry's group switch, rendered beside the descriptor's group.
GROUP_MODE = "CAPYCTL_GROUP_MODE"
# ADR 0028 §10: the member's own peer address, the one address SGLang reads.
GROUP_ADDRESS = "SGLANG_HOST_IP"
# CapyCTL-owned single-rank rendezvous and address inputs a group member never
# runs with: the file store (and its host directory) and the NIC lookup.
_GROUP_STRIPPED = ("SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE", "CAPYCTL_RENDEZVOUS_DIR",
                   "SGLANG_LOCAL_IP_NIC")


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
    if type(settings) is not dict:
        _reject()
    _exact_object(settings, _SETTINGS + tuple(name for name in _OPTIONAL_SETTINGS
                                              if name in settings))
    if "max_mamba_cache_size" in settings:
        _integer(settings["max_mamba_cache_size"], 1, _I32)
    if "group" in settings:
        _validate_group(settings["group"])
    if "draft_model_path" in settings:
        draft = _text(settings["draft_model_path"], 4096)
        if not draft.startswith("/") or any(part in (".", "..")
                                            for part in draft.split("/")):
            _reject()
    if "lmcache_config_file" in settings:
        config = _text(settings["lmcache_config_file"], 4096)
        # ADR 0033 §2: one private server on one machine; never a group member.
        if (not config.startswith("/") or "group" in settings
                or any(part in (".", "..") for part in config.split("/"))):
            _reject()
    if "trace_endpoint" in settings:
        endpoint = _text(settings["trace_endpoint"], 4096)
        # ADR 0037 §7: an http(s) URL without whitespace; never a group
        # member (groups run without engine tracing).
        if (not endpoint.startswith(("http://", "https://")) or "group" in settings
                or any(char.isspace() for char in endpoint)):
            _reject()
    for name in _PARSER_SETTINGS:
        if name in settings:
            value = settings[name]
            if type(value) is not str or value.startswith("-"):
                _reject()
            _optional_token(value)
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
    # ADR 0014 A17: a park that keeps the weights mapped needs the saver and
    # no weights backup.
    if (settings["weight_restore"] != "resident" or settings["memory_saver"] is not True
            or settings["cpu_weight_backup"] is not False):
        _literal(settings["weight_restore"], expected_restore)
    memory = settings["memory"]
    keys = ("request_bytes", "kv_cache_bytes", "margin_bytes", "static_bytes")
    # Discrete GPU design §6: the card's total is stated only on a discrete
    # device, where the static pool is a share of the card, not MemAvailable.
    discrete = type(memory) is dict and "device_total_bytes" in memory
    # ADR 0014, note on amendment A14: a discrete launch whose pools capyctl
    # fixed carries the allowance its fraction takes beyond the static pool.
    allowance = discrete and "static_allowance_bytes" in memory
    _exact_object(memory, keys + ("device_total_bytes",)
                  + (("static_allowance_bytes",) if allowance else ())
                  if discrete else keys)
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
        if allowance:
            _integer(memory["static_allowance_bytes"], 1, 1 << 34)
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


def _address(value):
    """One literal IP address, as Python's own parser reads it; never a name."""
    try:
        return ipaddress.ip_address(_text(value, 64))
    except ValueError:
        _reject()


def _validate_group(group):
    """ADR 0028 §10: one member's closed group object.

    Shape only: the agent checked the plan, and construct_server_args rechecks
    SGLang's resolution of the five reserved fields against it (group_drift).
    """
    optional = (GROUP_IFNAME_KEY,) if type(group) is dict and GROUP_IFNAME_KEY in group else ()
    _exact_object(group, _GROUP_KEYS + optional)
    for name in ("tp_size", "pp_size"):
        _integer(group[name], 1, 1024)
    _integer(group["nnodes"], 2, 1024)
    _integer(group["node_rank"], 0, group["nnodes"] - 1)
    # ADR 0028 §2: the members split the ranks evenly.
    if group["tp_size"] * group["pp_size"] % group["nnodes"]:
        _reject()
    # The head's rendezvous address, `<ip>:<port>` (an IPv6 head bracketed).
    host, _, port = _text(group["dist_init_addr"], 64).rpartition(":")
    bracketed = host.startswith("[") and host.endswith("]")
    if _address(host[1:-1] if bracketed else host).version != (6 if bracketed else 4):
        _reject()
    if (not port.isascii() or not port.isdigit() or str(int(port)) != port
            or not 1 <= int(port) <= 65535):
        _reject()
    _address(group["host_ip"])
    if optional:
        # R11: a kernel interface name (IFNAMSIZ less its terminator).
        name = _text(group[GROUP_IFNAME_KEY], 15)
        if not name.isascii() or any(not (char.isalnum() or char in "_-.") for char in name):
            _reject()


def _validate_selection(device):
    _exact_object(device, ("host_id", "hardware_fingerprint", "device_id", "memory_domain"))
    for selector in device.values():
        _text(selector, 256)
        if not selector.isascii() or any(not (char.isalnum() or char in "_-.:")
                                         for char in selector):
            _reject()


def _validate_devices(value):
    """ADR 0035 §4: a launch on several GPUs of one host names each one in
    rank order, the first being `device`: 2 to 64 distinct GPUs of the same
    host, never beside a group (ADR 0035 §12)."""
    devices = value["devices"]
    if (type(devices) is not list or not 2 <= len(devices) <= 64
            or "group" in value["settings"]):
        _reject()
    for device in devices:
        _validate_selection(device)
    if (devices[0] != value["device"]
            or len({device["device_id"] for device in devices}) != len(devices)
            or any(device["host_id"] != value["device"]["host_id"]
                   or device["hardware_fingerprint"] != value["device"]["hardware_fingerprint"]
                   for device in devices)):
        _reject()


def _validate_public(value):
    # ADR 0008 (owner decision 2026-09-23): no source revision token. The
    # installed SGLang build is identified by the host's installation
    # fingerprint and probed by shape (engine_capabilities.py), never pinned.
    # ADR 0035 §4: `devices` only on a launch on several GPUs.
    keys = ("schema_version", "kind", "engine", "binding_id", "incarnation", "endpoint",
            "served_name", "rendered_settings_digest", "settings", "device")
    _exact_object(value, keys + (("devices",) if type(value) is dict and "devices" in value
                                 else ()))
    for key, expected in (("schema_version", 2), ("kind", "sglang_launch"),
                          ("engine", "sglang")):
        _literal(value[key], expected)
    _ulid(value["binding_id"])
    _ulid(value["incarnation"])
    _validate_selection(value["device"])
    # The served name is the deployment's route (Spec §3), not a derived
    # binding artifact: an ASCII printable token, a non-empty run of at most
    # 256 bytes restricted to 0x21..=0x7E. This mirrors the coordinator's
    # `served_name_token` in crates/capyctl-adapters/src/sglang/args.rs, so both
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
    if "devices" in value:
        _validate_devices(value)


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
        # ADR 0028 §10, ADR 0012: a group worker (node_rank > 0) serves no API
        # and is handed no API credential: the private launch descriptor, and,
        # when its residency is deep, its own observation credential (ADR 0028
        # §12). Every other launch carries both credential descriptors, exactly
        # as before.
        worker_names = ("--public-settings-json", "--launch-descriptor-fd",
                        "--observation-credential-fd")
        if type(argv) not in (list, tuple) or len(argv) not in (4, 6, 8):
            _reject()
        names = names if len(argv) == 8 else worker_names[:len(argv) // 2]
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
        worker = public["settings"].get("group", {}).get("node_rank", 0) > 0
        if (worker != (len(argv) != 8)
                or (worker and (len(fds) == 2) != (public["settings"]["memory_saver"] is True))):
            _reject()
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
        if worker:
            observation = _credential(descriptor_reader(fds[1])) if len(fds) == 2 else None
            return LaunchSpec(json.dumps(public, sort_keys=True, separators=(",", ":")),
                              root, None, None, launch_scope, digest, observation)
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


def _compose_search_path():
    """Append the verified installation's search entries, never site.main().

    SPEC §9.2, ADR 0008 amendment 2026-10-19: the launcher-selected
    site-packages and, when it is a real directory inside that tree, its
    nvidia-cutlass-dsl package directory (sglang_startup_guards
    .trusted_search_path), the one path a vendor .pth hook adds that SGLang
    needs; the hook itself never runs. Appended, so the stdlib and this
    protected package keep precedence over the verified tree. SGLang's
    scheduler and detokenizer are spawned, and CPython spawn hands each child
    this sys.path before it runs the protected main script, so they import
    from the same composition.
    """
    from runtime.sglang_startup_guards import trusted_search_path
    site_packages = os.path.dirname(os.path.dirname(_trusted_package_root()))
    for search in trusted_search_path(site_packages):
        if search not in sys.path:
            sys.path.append(search)


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

    ADR 0035 §4: on several GPUs the inherited namespace is their UUIDs in
    rank order; the first is the mapping's UUID and all of them its list.
    """
    from runtime.sglang_device import TrustedDeviceMapping
    public = json.loads(spec._public_json)
    device = public["device"]
    visible = os.environ.get("CUDA_VISIBLE_DEVICES")
    several = "devices" in public
    uuids = tuple(visible.split(",")) if several and type(visible) is str else ()
    return TrustedDeviceMapping(
        host_id=device["host_id"], hardware_fingerprint=device["hardware_fingerprint"],
        device_id=device["device_id"], memory_domain=device["memory_domain"],
        physical_gpu_uuid=uuids[0] if uuids else visible,
        inventory_digest=digest, physical_gpu_uuids=uuids)


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
    _compose_search_path()
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
    _compose_search_path()
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
    # ADR 0028 §10: group mode is the descriptor's group and the rendered
    # switch together; either alone is a launch this boundary does not know.
    group = json.loads(spec._public_json)["settings"].get("group")
    if (os.environ.get(GROUP_MODE) == "1") != (group is not None):
        raise LaunchError("invalid_descriptor")
    if group is None:
        # SPEC §8.2 / T21: pinned before any engine import, so no engine
        # module reads a TCP rendezvous (torch's TCPStore binds every interface).
        try:
            rendezvous = loopback_rendezvous.pin("sglang")
        except loopback_rendezvous.RendezvousError:
            raise LaunchError("loopback_rendezvous_failed") from None
    else:
        # ADR 0028 §10: a member rendezvouses over TCP at the head's peer
        # address (`dist_init_addr`); no file store, no inherited transport.
        prepare_group_environment(os.environ, group.get(GROUP_IFNAME_KEY))
    arguments, launch = _guarded_engine_import()
    _require_capabilities(spec, arguments, launch)
    try:
        checked = sglang_server_args.construct_server_args(
            spec, contract.placement, arguments.ServerArgs, approvals=approvals)
    except sglang_server_args.ServerArgsError as error:
        # ADR 0014 §6: the mapper's closed category reaches the operator.
        raise LaunchError(error.code) from None
    if json.loads(spec._public_json)["settings"]["memory_saver"] is True:
        # The saver preloads in a fresh interpreter, before torch can locate
        # wheel-shipped libcudart. Compose its loader path before any spawn.
        from runtime.sglang_startup_guards import prepare_saver_library_path
        prepare_saver_library_path(os.path.dirname(os.path.dirname(_trusted_package_root())))
        # Only a memory-saver launch is ever woken by a disk reload.
        _keep_served_name_on_reload()
    # ADR 0028 §12 (R12): every rank, head and worker alike, enrolls its own
    # saver observation when the residency is deep.
    target = _observation_target(spec, launch)
    if group is None:
        try:
            # Rechecked last: the scheduler inherits exactly what was verified.
            loopback_rendezvous.verify(rendezvous)
        except loopback_rendezvous.RendezvousError:
            raise LaunchError("loopback_rendezvous_failed") from None
    else:
        verify_group_environment(os.environ, group)
    # ADR 0028 §9: readiness is the head's. A rank > 0 runs SGLang's health
    # server on its loopback worker port and nothing probes it: its health
    # always passes.
    options = {} if target is None else {"run_scheduler_process_func": target}
    shared_memory = _SharedMemoryWatch()
    shared_memory.start()
    ending = _LaunchEnd(shared_memory)
    ending.install()
    try:
        launch.launch_server(checked._native, **options)
    except Exception:
        # SPEC §§6.4, 13.2 (found live 2026-10-20): a scheduler that exits
        # before readiness (a missing CUDA library, an import failure in the
        # spawned child) sends no SIGQUIT; SGLang's readiness wait notices it
        # within seconds and raises here. The processes it already started
        # (the detokenizer, any live scheduler) are non-daemon children that
        # interpreter exit would join forever, holding this entry, and so the
        # launch, alive until the host's readiness deadline. They are ended and
        # seen gone first: the entry leaves only once nothing it started runs.
        # Then the shared memory they left in /dev/shm (RAM) is removed.
        ending.run()
        raise LaunchError("engine_launch_failed") from None
    else:
        # SPEC §13.2 (found live 2026-10-22, SGLang 0.5.22): on the host's
        # SIGTERM SGLang drains, kills its process tree without this entry,
        # and `launch_server` returns: the launch is stopped. It ends here as
        # a failed one does, so its shared memory is removed too.
        ending.run()
    finally:
        ending.uninstall()


def _process_table():
    """Every process /proc lists now: pid -> (parent pid, state letter)."""
    table = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open("/proc/" + name + "/stat", "rb") as handle:
                raw = handle.read()
        except OSError:
            continue  # Exited during the scan.
        # The command name may hold spaces and parentheses; the fields after
        # its last ')' are the state, then the parent pid.
        fields = raw[raw.rindex(b")") + 2:].split()
        table[int(name)] = (int(fields[1]), fields[0].decode("ascii"))
    return table


def _running_descendants(table):
    """This process's descendants that have not exited (zombies excluded)."""
    children = {}
    for pid, (parent, state) in table.items():
        if state not in ("Z", "X"):
            children.setdefault(parent, []).append(pid)
    found, frontier = [], [os.getpid()]
    while frontier:
        below = children.get(frontier.pop(), ())
        found.extend(below)
        frontier.extend(below)
    return found


def _signal(pid, number):
    try:
        os.kill(pid, number)
    except ProcessLookupError:
        pass


def _reap(table):
    """Collect this process's exited children, each by its owner first."""
    multiprocessing = sys.modules.get("multiprocessing")
    if multiprocessing is not None:
        # Each Process object reaps its own child and records its exit.
        multiprocessing.active_children()
    _stop_resource_tracker()
    for pid, (parent, state) in table.items():
        if parent == os.getpid() and state in ("Z", "X"):
            try:
                os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                pass  # Already reaped by its owner.


def _stop_resource_tracker():
    """Have multiprocessing's resource tracker reap its own (ended) process.

    Found live 2026-10-22: reaped by `_reap` instead, the tracker's finalizer
    at interpreter exit waited for a child already gone, and Python 3.12
    printed its `ChildProcessError` after the failure line, so the line the
    host shows was no longer the log's last. Stopping it closes its pipe and
    waits for its pid; one never started, or already stopped, is left alone.
    """
    module = sys.modules.get("multiprocessing.resource_tracker")
    stop = getattr(getattr(module, "_resource_tracker", None), "_stop", None)
    if stop is None:
        return
    try:
        stop()
    except ChildProcessError:
        pass  # Already reaped; its pipe is closed, so its finalizer does nothing.


def _end_engine_processes(pause=0.05):
    """SPEC §13.2: end everything this entry started; return once none runs.

    The tree is frozen before it is killed, so no member forks a process that
    outlives its parent unseen: every running descendant is stopped, and the
    tree is read again until a pass finds none that is not stopped. Then each
    is killed, the entry's own children are reaped, and the tree is read again.
    This returns only once /proc shows no running descendant; a member that
    will not die keeps the entry, and so the launch's recorded ownership,
    alive, and the host's readiness deadline reports that, never a release.
    """
    while True:
        try:
            if _end_pass(pause):
                return
        except (OSError, ValueError):
            pass  # /proc unreadable: nothing is seen gone, so nothing is.
        time.sleep(pause)


def _end_pass(pause):
    """Freeze, kill and reap the running tree once; True if none runs after."""
    stopped = set()
    while True:
        table = _process_table()
        unstopped = [pid for pid in _running_descendants(table)
                     if pid not in stopped or table[pid][1] not in ("T", "t")]
        if not unstopped:
            break
        for pid in unstopped:
            if pid not in stopped:
                _signal(pid, signal.SIGSTOP)
                stopped.add(pid)
        time.sleep(pause)
    for pid in stopped:
        _signal(pid, signal.SIGKILL)
    time.sleep(pause)
    _reap(_process_table())
    return not _running_descendants(_process_table())


# Where SGLang, Python's multiprocessing and NCCL keep shared memory: RAM.
SHM_DIRECTORY = "/dev/shm"
# How often the watch lists that directory for a file the launch created.
_SHM_WATCH_INTERVAL = 0.25
# How many listings a new file may go unclaimed before it is taken as not the
# launch's (a creator opens the file it creates, so it is held from the start).
_SHM_CLAIM_ATTEMPTS = 3


class _SharedMemoryWatch:
    """SPEC §13.2: the shared memory a launch leaves behind is removed.

    Found live 2026-10-22: a scheduler killed while loading left
    `/dev/shm/sglang_loads_*.shm` behind. SGLang never removes that file, and
    the resource tracker that removes registered segments is killed with
    everything else, so each failed launch kept its segments in RAM; found
    again 2026-10-23 after normal stops. The watch runs for as long as
    `launch_server` does, and `_LaunchEnd` releases what it claimed when the
    launch fails or is stopped.

    Ownership is evidence, never a name pattern: a file is the launch's only
    when it appeared in the directory after the launch began and, when first
    seen, one of the launch's processes (this entry or a descendant) held it
    open or mapped. After the verified exit (`_end_engine_processes`), a
    claimed file is removed only if it is still the same file, owned by this
    user and private to it (no group or other access), and no live process
    other than this exiting entry holds it. Every process /proc lets this
    user inspect is checked. One that hides its descriptors and mappings (a
    non-dumpable process) keeps every file when it runs in this entry's
    process group, where it could be one of the launch's own that left this
    entry's tree; elsewhere (an SSH session, the user's service manager) it
    could only have opened a file by its new name, and is not checked. A
    zombie, the entry's own unreaped children included, holds nothing. A
    file whose creator ended within one listing interval of creating it is
    never claimed, and so stays.
    """

    def __init__(self, directory=SHM_DIRECTORY, interval=_SHM_WATCH_INTERVAL):
        self._directory = directory
        self._interval = interval
        self._before = set(self._names())
        self._unclaimed = {}  # name -> listings it went unclaimed
        self._claimed = {}  # (st_dev, st_ino) -> name
        self._stopped = threading.Event()
        self._thread = threading.Thread(target=self._run, name="capyctl-shm-watch",
                                        daemon=True)

    def start(self):
        self._thread.start()

    def stop(self):
        """Stop watching; the launch returned or raised."""
        self._stopped.set()
        self._thread.join()

    def claim(self):
        """Claim once more, every new file again, while the processes run."""
        self._observe(final=True)

    def _names(self):
        try:
            return os.listdir(self._directory)
        except OSError:
            return []

    def _run(self):
        while not self._stopped.wait(self._interval):
            self._observe()

    def _observe(self, final=False):
        try:
            names = set(self._names())
            self._before &= names  # A removed name is never seen as old again.
            fresh = names - self._before
            for name in [name for name in self._unclaimed if name not in fresh]:
                del self._unclaimed[name]
            pending = {}
            for name in fresh:
                attempts = self._unclaimed.get(name, 0)
                if attempts >= _SHM_CLAIM_ATTEMPTS and not final:
                    continue
                key = self._identity(name)
                if key is not None and key not in self._claimed:
                    pending[key] = name
            if not pending:
                return
            launch = _running_descendants(_process_table()) + [os.getpid()]
            held, _ = _held_files(self._directory, launch)
            for key, name in pending.items():
                if key in held:
                    self._claimed[key] = name
                    self._unclaimed.pop(name, None)
                    self._before.add(name)  # Claimed: not examined again.
                else:
                    self._unclaimed[name] = self._unclaimed.get(name, 0) + 1
        except (OSError, ValueError):
            pass  # /proc or the directory unreadable: nothing is claimed.

    def _identity(self, name):
        """(device, inode) of a regular file this user owns privately, or None."""
        try:
            status = os.lstat(os.path.join(self._directory, name))
        except OSError:
            return None
        if (not stat.S_ISREG(status.st_mode) or status.st_uid != os.geteuid()
                or status.st_mode & 0o077):
            return None
        return status.st_dev, status.st_ino

    def release(self):
        """Remove each claimed file no live process other than this one holds."""
        if not self._claimed:
            return
        try:
            others = [pid for pid in _process_table() if pid != os.getpid()]
            held, uncertain = _held_files(self._directory, others, group=os.getpgrp())
        except (OSError, ValueError):
            return  # Nothing is seen unheld, so nothing is removed.
        if uncertain:
            return
        finalizers = _semaphore_finalizers()
        for key, name in self._claimed.items():
            if key in held or self._identity(name) != key:
                continue
            try:
                os.unlink(os.path.join(self._directory, name))
            except OSError:
                continue  # Removed by another, or not removable: left as it is.
            if name in finalizers:
                # Removed for it: at a normal exit its finalizer would not
                # find the file and print a traceback after the entry's
                # last line.
                finalizers[name].cancel()


def _semaphore_finalizers():
    """This process's pending multiprocessing semaphore finalizers, by file name.

    A semaphore this entry made (`SemLock`, spawn context) unlinks its
    `/dev/shm/sem.<name>` file in a finalizer that runs at a normal exit.
    """
    util = sys.modules.get("multiprocessing.util")
    synchronize = sys.modules.get("multiprocessing.synchronize")
    cleanup = getattr(getattr(synchronize, "SemLock", None), "_cleanup", None)
    if util is None or cleanup is None:
        return {}
    found = {}
    for finalizer in list(getattr(util, "_finalizer_registry", {}).values()):
        arguments = getattr(finalizer, "_args", None)
        if getattr(finalizer, "_callback", None) is cleanup and arguments:
            found["sem." + str(arguments[0]).lstrip("/")] = finalizer
    return found


def _held_files(directory, pids, group=None):
    """The (device, inode) of every file in `directory` the `pids` hold.

    A file is held when a process has it open or mapped. Returns the set and
    whether a process of this user in process group `group` could not be
    inspected, and so may hold any.
    """
    # Mappings and descriptors name the resolved path (/dev/shm may be a link).
    prefix = os.path.realpath(directory).rstrip("/") + "/"
    held, uncertain = set(), False
    for pid in pids:
        base = "/proc/%d/" % pid
        try:
            with open(base + "maps", "rb") as handle:
                for line in handle:
                    fields = line.split(None, 5)
                    if len(fields) == 6 and fields[5].startswith(prefix.encode()):
                        major, minor = fields[3].split(b":")
                        held.add((os.makedev(int(major, 16), int(minor, 16)),
                                  int(fields[4])))
            for descriptor in os.listdir(base + "fd"):
                path = base + "fd/" + descriptor
                try:
                    if os.readlink(path).startswith(prefix):
                        status = os.stat(path)
                        held.add((status.st_dev, status.st_ino))
                except OSError:
                    continue  # Closed during the scan.
        except PermissionError:
            uncertain = uncertain or (group is not None and _may_hide_ours(pid, group))
        except (FileNotFoundError, ProcessLookupError):
            continue  # Exited during the scan.
    return held, uncertain


def _may_hide_ours(pid, group):
    """Whether a process /proc hides could still hold one of the launch's files.

    Only a live process of this user in the launch's process group could: a
    zombie's descriptors and mappings went at its exit.
    """
    try:
        with open("/proc/%d/stat" % pid, "rb") as handle:
            raw = handle.read()
        fields = raw[raw.rindex(b")") + 2:].split()
        if fields[0] in (b"Z", b"X") or int(fields[2]) != group:
            return False
        with open("/proc/%d/status" % pid, "rb") as handle:
            for line in handle:
                if line.startswith(b"Uid:"):
                    return os.geteuid() in [int(field) for field in line.split()[1:]]
    except (OSError, ValueError, IndexError):
        pass  # Exited during the scan.
    return False


class _LaunchEnd:
    """SPEC §13.2: a launch that fails or is stopped leaves no shared memory.

    Found live 2026-10-22: every SGLang 0.5.22 launch, normal stops included,
    left `/dev/shm/sglang_loads_*.shm` and a multiprocessing semaphore
    (`sem.mp-*`) behind; only a failed launch removed them. The host's
    SIGTERM reaches this entry (the group's leader) and every process of the
    group, and then

    - while SGLang loads, SIGTERM's default action ended this entry at once;
    - once SGLang serves, its handler drains, kills its process tree without
      this entry and `launch_server` returns (the usual stop); or, after a
      child's exit, its SIGQUIT handler kills the tree and then this entry
      with SIGKILL (`kill_process_tree`, through psutil's `os.kill`), which
      no finalizer survives.

    So the launch ends here, once, on whichever comes first: `launch_server`
    returning or raising, a SIGTERM this entry handles itself while
    `launch_server` runs (SGLang's own handler replaces it once it serves),
    or this process's SIGKILL to itself, which its `os.kill` intercepts. The
    watch claims once more, every process the entry started is ended and seen
    gone (`_end_engine_processes`), and only then is each claimed file no
    other live process holds removed (`_SharedMemoryWatch.release`). The
    SIGTERM then ends the entry as before, and the SIGKILL is delivered. A
    stop the host escalates to SIGKILL ends the entry with no chance to run,
    and its files stay.
    """

    def __init__(self, shared_memory):
        self._shared_memory = shared_memory
        self._pid = os.getpid()
        self._kill = os.kill
        self._lock = threading.RLock()
        self._state = "launched"  # then "ending", then "ended"
        self._installed_sigterm = False
        self._previous_sigterm = signal.SIG_DFL

    def install(self):
        """Take SIGTERM and this process's SIGKILL to itself."""
        os.kill = self._guarded_kill
        try:
            self._previous_sigterm = signal.signal(signal.SIGTERM, self._terminated)
            self._installed_sigterm = True
        except ValueError:
            pass  # Not the main thread: SIGTERM keeps its handler.

    def uninstall(self):
        """Hand both back: `launch_server` returned or raised."""
        if os.kill == self._guarded_kill:
            os.kill = self._kill
        if self._installed_sigterm and signal.getsignal(signal.SIGTERM) == self._terminated:
            signal.signal(signal.SIGTERM, self._previous_sigterm)

    def run(self):
        """End the launch's processes, verified, then remove its shared memory."""
        with self._lock:
            if self._state != "launched":
                return
            self._state = "ending"
            self._shared_memory.stop()
            self._shared_memory.claim()
            _end_engine_processes()
            self._shared_memory.release()
            self._state = "ended"

    def _terminated(self, number, frame):
        if os.getpid() != self._pid:
            # A child forked while this handler was in place (a worker pool):
            # SIGTERM ends it as it did before; the launch is this entry's.
            signal.signal(signal.SIGTERM, signal.SIG_DFL)
            self._kill(os.getpid(), signal.SIGTERM)
            return
        if self._state == "ending":
            return  # The launch is already ending; the entry exits after.
        self.run()
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        self._kill(self._pid, signal.SIGTERM)

    def _guarded_kill(self, pid, number):
        if number == signal.SIGKILL and pid == self._pid == os.getpid():
            self.run()
        return self._kill(pid, number)


def prepare_group_environment(environ, expected_ifname=None):
    """ADR 0028 §10 (R11): strip a group member's inherited transport inputs.

    Every `NCCL_*`, `MASTER_*` and `GLOO_*` name goes, except the resolved
    `GLOO_SOCKET_IFNAME` (`expected_ifname`), as do `HOST_IP` and the
    single-rank file rendezvous (`SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE`,
    its host directory) and `SGLANG_LOCAL_IP_NIC`. `SGLANG_HOST_IP` stays as
    rendered; `verify_group_environment` checks it.
    """
    from runtime import loopback_rendezvous
    try:
        loopback_rendezvous.pin_group(environ, GROUP_ADDRESS, expected_ifname)
    except loopback_rendezvous.RendezvousError:
        raise LaunchError("loopback_rendezvous_failed") from None
    for name in _GROUP_STRIPPED:
        environ.pop(name, None)


def verify_group_environment(environ, group):
    """Recheck, right before the engine starts, the member's environment."""
    from runtime import loopback_rendezvous
    try:
        loopback_rendezvous.verify_group(environ, GROUP_ADDRESS, group["host_ip"],
                                         group.get(GROUP_IFNAME_KEY))
    except loopback_rendezvous.RendezvousError:
        raise LaunchError("loopback_rendezvous_failed") from None
    if any(name in environ for name in _GROUP_STRIPPED):
        raise LaunchError("loopback_rendezvous_failed")


def _keep_served_name_on_reload():
    """SPEC §3: the served name is capyctl's (the route name), and stays so.

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
    observation directory (CAPYCTL_OBSERVATION_DIR) and it validates, and only when
    the installation's `launch_server` takes a scheduler target (the
    `observation` capability, engine_capabilities.py). Anything else serves
    without an observation, and the host refuses Park unchanged (fail closed).

    ADR 0028 §12 (R12): every group rank enrolls alike. Its scope carries the
    group's topology, and a worker's target carries its observation
    credential (it holds no admin credential to derive the key from).
    """
    from runtime import engine_capabilities, sglang_observation_enrollment as enrollment
    # Children see only a scope this entry validated, never the raw input.
    directory = os.environ.get(enrollment.ENV_DIR)
    enrollment.clear_environment()
    public = json.loads(spec._public_json)
    settings = public["settings"]
    scope = (directory, public["binding_id"], public["incarnation"], settings["weight_restore"])
    group = settings.get("group")
    # ADR 0035 §9: every scheduler of a launch on several GPUs enrolls its
    # own saver map; the scope carries the plan's N.
    devices = public.get("devices")
    if group is not None:
        options = {"group": (group["tp_size"], group["pp_size"], group["nnodes"])}
    elif devices is not None:
        options = {"tensor_parallel": len(devices)}
    else:
        options = {}
    if (directory is None or settings["memory_saver"] is not True
            or not engine_capabilities.accepts_scheduler_target(launch)
            or not enrollment.entry_environment(*scope, **options)):
        return None
    return enrollment.scheduler_target(spec._observation_secret)


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
                " parking (deep or host_backed) needs; declare residency restart_only,"
                " or use a build that provides them)")
    if code == "capability_missing:core":
        return " (this SGLang installation lacks an interface every launch needs)"
    if code == "engine_launch_failed":
        return (" (SGLang's launch failed after it started its processes, for example a"
                " scheduler that exited before readiness; every one was ended, and the"
                " engine's own lines above say why)")
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
