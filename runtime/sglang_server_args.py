"""Pure ServerArgs mapping for SGLang 94602c9c2b7cbdb8efd5c52802dac6a1c180089e.

ADR 0014 §2, §3, §4, §6: a deployment's typed engine settings and its extra
arguments flow into ServerArgs; only the settings capyctl owns (the reserved
subset) are fixed, and they are checked again after SGLang's own resolution.

No SGLang imports, device access, or launch authority live here. In particular,
ServerArgs.resolve_once is NOT pure: it calls platform plugin hooks, reads model
configuration, queries GPU capacity, and propagates environment variables. A
production caller must establish source/package, logging, plugin, environment
and physical-placement guards BEFORE importing/calling its constructor.
The protected entrypoint calls this mapper only after those gates pass.

Checks cover the reserved subset (ADR 0014 §3), the security-gated
trust_remote_code switch and, when the deployment turned CUDA graphs off, the
resolved graph phases. They do not attest unlisted backend defaults, compiled
kernels, complete worker enrollment, the actual saver implementation, or
observed allocation bounds. Whether the engine supports the requested
combination on this checkpoint is the user's responsibility (ADR 0011).
"""

import argparse
from dataclasses import dataclass, field
import json
import os
import sys

from . import extra_args_policy
from . import sglang_entry


# ADR 0028 §10: the reserved fields a multi-node group member takes from its
# descriptor's `group` object instead of the single-rank constants below.
GROUP_FIELDS = ("tp_size", "pp_size", "nnodes", "node_rank", "dist_init_addr")


class ServerArgsError(Exception):
    """Sanitized boundary error; never render native arguments or exceptions."""

    def __init__(self, code):
        if code not in ("invalid_launch_inputs", "placement_mismatch",
                        "server_args_construction_failed", "effective_args_mismatch",
                        "invalid_extra_args", "memory_grant_unavailable",
                        "sensitive_option_refused",
                        *("group_drift:" + name for name in GROUP_FIELDS)):
            code = "effective_args_mismatch"
        super().__init__(code)
        self.code = code


@dataclass(frozen=True, repr=False)
class ObservedPlacement:
    """Trusted local collector input, NOT a request DTO or proof by construction.

    The collector must freshly corroborate host/hardware and logical selection
    against the physical UUID, plus the ordered CUDA-visible namespace actually
    inherited by this process and its children. cuda_index addresses that namespace,
    not an nvidia-smi ordinal. This type has no JSON/assertion ingestion path.
    Reconstructing it cannot grant admission or establish collector trust.
    """

    binding_id: str
    incarnation: str
    host_id: str
    hardware_fingerprint: str
    device_id: str
    memory_domain: str
    physical_gpu_uuid: str
    cuda_visible_uuids: tuple
    cuda_index: int

    def __repr__(self):
        return "ObservedPlacement(<local observation>)"


# ADR 0014 §3: the ServerArgs fields capyctl owns, with the constant value each
# takes. Fields rendered from the binding, grant or residency are added in
# construct_server_args. Names were read from the installed 0.5.20 tree
# (arg_groups/fields/*.py). A reserved name missing from the resolved record
# fails closed (ADR 0014 open issue 4): drift is visible, never silent.
_RESERVED_CONSTANT = {
    "revision": None, "device": "cuda", "gpu_id_step": 1,
    "tp_size": 1, "dp_size": 1, "pp_size": 1, "ep_size": 1,
    "dcp_size": 1, "attn_cp_size": 1, "moe_dp_size": 1,
    "nnodes": 1, "node_rank": 0, "dist_init_addr": None, "use_ray": False,
    # SPEC §8.2 / T21: the rendezvous is capyctl's file store
    # (loopback_rendezvous.py); no TCP rendezvous port is chosen or bound.
    "nccl_port": None,
    "enable_dp_attention": False, "cpu_offload_gb": 0,
    "enable_draft_weights_cpu_backup": False,
    "grpc_port": None, "grpc_mode": False, "smg_grpc_mode": False,
    "sidecar": None, "sidecar_args": None, "smg_http_sidecar_port": None,
    "fastapi_root_path": "", "enable_http2": False,
    "ssl_keyfile": None, "ssl_certfile": None, "ssl_ca_certs": None,
    "ssl_keyfile_password": None, "enable_ssl_refresh": False,
    # These levels suppress ordinary argument info logs, NOT a complete logging
    # guard: native code formats f"{server_args=}" before logger filtering.
    "log_level": "error", "log_level_http": "error", "log_requests": False,
    "log_requests_target": None, "crash_dump_folder": None,
    # W8: the host agent scrapes loopback /metrics for load reports.
    "enable_metrics": True,
    "skip_server_warmup": True,
    "disaggregation_mode": "null",
    "enable_hierarchical_cache": False, "hicache_storage_backend": None,
    "hicache_storage_backend_extra_config": None,
    "enable_lmcache": False, "lmcache_config_file": None,
    "enable_flexkv": False, "flexkv_config_file": None,
    # SPEC §1.2: quantization is out of scope; serving a quantized checkpoint
    # is the typed `quantization` field, not modelopt export.
    "quantize_and_serve": False, "modelopt_checkpoint_restore_path": None,
    "modelopt_checkpoint_save_path": None, "modelopt_export_path": None,
}
_RESERVED_BOUND = ("host", "port", "api_key", "admin_api_key", "model_path",
                   "tokenizer_path", "served_model_name", "base_gpu_id",
                   "mem_fraction_static", "enable_memory_saver",
                   "enable_weights_cpu_backup")
# Whole reserved families (ADR 0014 §3), matched on the parsed destination.
_RESERVED_FAMILIES = ("ssl_", "modelopt_", "disaggregation_", "hicache_storage_",
                      "lmcache_", "flexkv_")
# Destinations an extra argument may never set: the reserved subset, the
# configuration-file option (SPEC §8.2: a file would hide values), and each
# typed field's native destinations (ADR 0014 §2: one way to say each thing).
_TYPED_DESTINATIONS = frozenset({
    "dtype", "quantization", "kv_cache_dtype", "context_length",
    "max_running_requests", "disable_cuda_graph", "disable_prefill_cuda_graph",
    "disable_decode_cuda_graph", "cuda_graph_backend_prefill",
    "cuda_graph_backend_decode", "language_model_only", "trust_remote_code",
    "max_total_tokens", "chunked_prefill_size", "tokenizer_worker_num",
})
_FORBIDDEN_EXTRA = frozenset({*_RESERVED_CONSTANT, *_RESERVED_BOUND, "config"})
# ADR 0014 §4: capyctl safe defaults the deployment may override with extra args.
_SAFE_DEFAULTS = {"detokenizer_worker_num": 1}


def _validated_public(spec):
    try:
        if type(spec) is not sglang_entry.LaunchSpec:
            raise ValueError()
        public = json.loads(spec._public_json)
        private = json.dumps({"schema_version": 1,
                              "kind": "sglang_private_launch",
                              "checkpoint_root": spec._checkpoint_root,
                              "public_settings": public}).encode()
        argv = ["--public-settings-json", spec._public_json, "--launch-descriptor-fd", "3"]
        payloads = {3: private}
        # ADR 0028 §10: a group worker's launch carries no credential.
        if spec._inference_key is not None or spec._admin_key is not None:
            argv += ["--inference-credential-fd", "4", "--admin-credential-fd", "5"]
            payloads.update({4: spec._inference_key.encode("ascii"),
                             5: spec._admin_key.encode("ascii")})
        # Reuse the entire strict boundary: LaunchSpec's Python constructor is
        # not itself a validation capability. No descriptor I/O occurs here.
        sglang_entry.build_launch(argv, payloads.__getitem__)
        return public
    except Exception:
        raise ServerArgsError("invalid_launch_inputs") from None


def _check_placement(public, placement):
    try:
        if type(placement) is not ObservedPlacement:
            raise ValueError()
        for key in ("binding_id", "incarnation"):
            if getattr(placement, key) != public[key]:
                raise ValueError()
        for key, expected in public["device"].items():
            if getattr(placement, key) != expected:
                raise ValueError()
        visible = placement.cuda_visible_uuids
        index = placement.cuda_index
        if (type(visible) is not tuple or not visible or len(visible) > 256
                or type(index) is not int or not 0 <= index < len(visible)
                or len(set(visible)) != len(visible)):
            raise ValueError()
        for uuid in visible:
            if (type(uuid) is not str or not 0 < len(uuid) <= 256
                    or not uuid.isascii()
                    or any(not (c.isalnum() or c in "-_") for c in uuid)):
                raise ValueError()
        if (type(placement.physical_gpu_uuid) is not str
                or visible[index] != placement.physical_gpu_uuid):
            raise ValueError()
    except Exception:
        raise ServerArgsError("placement_mismatch") from None


def available_memory_bytes(meminfo="/proc/meminfo"):
    """The baseline SGLang sizes its static pool against, read without imports.

    On an integrated device (GB10 unified memory) SGLang 0.5.20's
    get_available_gpu_memory reads psutil's system `available`, which is
    /proc/meminfo MemAvailable. This is read before the engine starts, so a
    later release by another engine can raise SGLang's own reading; the
    resolved fraction is still the one capyctl rendered. A discrete device sizes
    against its own total instead (`available_bytes_for`).
    """
    try:
        with open(meminfo, "rb") as stream:
            for line in stream.read(65536).decode("ascii").splitlines():
                name, _, rest = line.partition(":")
                if name == "MemAvailable":
                    value, unit = rest.split()
                    if unit != "kB":
                        raise ValueError()
                    return int(value) * 1024
    except Exception:
        pass
    raise ServerArgsError("memory_grant_unavailable") from None


def available_bytes_for(memory):
    """The baseline mem_fraction_static is a fraction of (discrete GPU design §6).

    ADR 0019: on a discrete device the launching host states the card's total
    (`device_total_bytes`, observed with the sample its launch check used) and
    SGLang's static pool is a share of the card. Unified memory keeps
    MemAvailable. A stated total that is not a positive integer is refused.
    """
    total = memory.get("device_total_bytes")
    if total is None:
        return available_memory_bytes()
    if type(total) is not int or not 0 < total < 2**62:
        raise ServerArgsError("memory_grant_unavailable")
    return total


def static_fraction(static_bytes, available_bytes):
    """ADR 0014 §3: mem_fraction_static rendered from the grant.

    Rounded down to basis points so the static pool never exceeds the grant's
    static share; a grant that does not fit below the whole baseline, or rounds
    to nothing, is refused rather than clamped.
    """
    if (type(static_bytes) is not int or type(available_bytes) is not int
            or static_bytes <= 0 or available_bytes <= 0):
        raise ServerArgsError("memory_grant_unavailable")
    bps = static_bytes * 10000 // available_bytes
    if not 1 <= bps <= 9999:
        raise ServerArgsError("memory_grant_unavailable")
    return bps / 10000


def fraction_bytes(memory, available_bytes):
    """The bytes mem_fraction_static is rendered from: the static pool.

    ADR 0014, note on amendment A14 (found live 2026-10-04): SGLang sizes its
    pools from the fraction times the GPU memory free when it starts (after
    its CUDA context, without the driver's reserve), not times the card's
    total, so a discrete launch whose pools capyctl fixed (KV tokens and state
    slots) adds `static_allowance_bytes`. The fixed pools bound what SGLang
    allocates; the fraction stays below the whole card.
    """
    static_bytes = memory["static_bytes"]
    allowance = memory.get("static_allowance_bytes")
    if allowance is None:
        return static_bytes
    if (type(allowance) is not int or allowance <= 0 or type(available_bytes) is not int
            or type(static_bytes) is not int):
        raise ServerArgsError("memory_grant_unavailable")
    return min(static_bytes + allowance, -(-available_bytes * 9999 // 10000))


class _ClosedParser(argparse.ArgumentParser):
    """The installed parser's options, without exit, help or usage output."""

    def error(self, message):
        raise ServerArgsError("invalid_extra_args")

    def exit(self, status=0, message=None):
        raise ServerArgsError("invalid_extra_args")


def parse_extra_args(extra_args, server_args_class, approvals=None, checkpoint=None):
    """ADR 0014 §6: parse extra arguments with the installed ServerArgs parser.

    Returns only the destinations the arguments actually supplied (every
    default is suppressed), so abbreviations, aliases and `=value` forms all
    arrive under the field name they resolve to. Any reserved destination,
    configuration file or typed-field destination is refused here, before
    construction; the post-resolution recheck remains the authority. ADR 0014
    §8 / SPEC §8.2: a sensitive destination (code, listener or egress, path,
    JSON configuration) needs the host's named approval, decided on the
    resolved destination, never the spelling (extra_args_policy).
    """
    if approvals is None:
        approvals = extra_args_policy.parse_approvals(None)
    if not extra_args:
        return {}
    try:
        parser = _ClosedParser(prog="sglang serve", add_help=False)
        server_args_class.add_cli_args(parser)
        for action in parser._actions:
            action.default = argparse.SUPPRESS
            action.required = False
        supplied = vars(parser.parse_args(list(extra_args)))
    except ServerArgsError:
        raise
    except BaseException:
        raise ServerArgsError("invalid_extra_args") from None
    for name in supplied:
        if (name in _FORBIDDEN_EXTRA or name.startswith(_RESERVED_FAMILIES)
                or name in _TYPED_DESTINATIONS):
            raise ServerArgsError("effective_args_mismatch")
    try:
        extra_args_policy.check("sglang", supplied, approvals, checkpoint)
    except extra_args_policy.Refused:
        raise ServerArgsError("sensitive_option_refused") from None
    # Mirror ServerArgs.from_cli_args: parser destinations that are not record
    # fields (deprecated switches) never reach the constructor.
    fields = getattr(server_args_class, "__struct_fields__", None)
    if fields is not None:
        supplied = {name: value for name, value in supplied.items() if name in fields}
    return supplied


def typed_keywords(settings):
    """ADR 0014 §2: the typed fields a deployment set, as ServerArgs keywords."""
    keywords = {"trust_remote_code": settings["trust_remote_code"],
                "tokenizer_worker_num": settings["tokenizer_workers"]}
    for name, native in (("dtype", "dtype"), ("quantization", "quantization"),
                         ("kv_cache_dtype", "kv_cache_dtype"),
                         ("context_length", "context_length"),
                         ("max_running_requests", "max_running_requests"),
                         ("max_total_tokens", "max_total_tokens"),
                         ("chunked_prefill_size", "chunked_prefill_size")):
        if settings[name] is not None:
            keywords[native] = settings[name]
    # ADR 0024: the parsers chosen at launch (SGLang 0.5.20 and 0.5.21 fields).
    # ADR 0014 amendment A14: the recurrent-state slots sized at launch.
    for name in ("tool_call_parser", "reasoning_parser", "max_mamba_cache_size"):
        if name in settings:
            keywords[name] = settings[name]
    if settings["language_model_only"]:
        keywords["language_model_only"] = True
    if settings["cuda_graphs"] is False:
        keywords.update(disable_prefill_cuda_graph=True, disable_decode_cuda_graph=True,
                        cuda_graph_backend_prefill="disabled",
                        cuda_graph_backend_decode="disabled")
    return keywords


@dataclass(frozen=True, repr=False)
class CheckedServerArgs:
    """Private native object plus retained explicit expectations, not authority.

    The guarded launcher may consume _native only after revalidate and all its
    independent checks. Never log _native: upstream dataclass repr includes keys.
    Revalidation does not independently recollect device or process observations.
    """

    _native: object = field(repr=False)
    _expected: tuple = field(repr=False)
    _graphs_disabled: bool = field(default=False, repr=False)
    # ADR 0028 §10: the group fields of a multi-node member, whose change is
    # named `group_drift:<field>`; empty for a single rank.
    _group_fields: tuple = field(default=(), repr=False)

    def __repr__(self):
        return "CheckedServerArgs(<private; not launch authority>)"

    def revalidate(self):
        try:
            resolved = self._native.resolved_dict()
        except Exception:
            raise ServerArgsError("effective_args_mismatch") from None
        wanted = dict(self._expected)
        # ADR 0028 §10: a group field SGLang's resolution changed or dropped
        # is refused by name, before anything else is compared.
        for key in self._group_fields:
            value = resolved.get(key, _MISSING) if type(resolved) is dict else _MISSING
            if type(value) is not type(wanted[key]) or value != wanted[key]:
                raise ServerArgsError("group_drift:" + key)
        try:
            # SPEC §8.1 / T22: 0.5.20 preserves raw fields; validate the resolved
            # projection consumed by the engine, not constructor declarations.
            for key, expected in self._expected:
                value = resolved[key]
                if type(value) is not type(expected) or value != expected:
                    raise ValueError()
            if self._graphs_disabled:
                config = resolved["cuda_graph_config"]
                for phase in (config["prefill"], config["decode"]):
                    if type(phase["backend"]) is not str or phase["backend"] != "disabled":
                        raise ValueError()
        except Exception:
            raise ServerArgsError("effective_args_mismatch") from None


_MISSING = object()


def _log_refusal(error):
    """SPEC §13.3 (found live 2026-10-03): SGLang's own reason for refusing the
    arguments, for the private engine log, only when the operator turned debug
    engine logs on (`--debug-engine-logs`); scrubbed of credentials and bounded.
    The raised category stays closed either way."""
    if os.environ.get("CAPYCTL_DEBUG_ENGINE_LOGS") != "1":
        return
    from .sglang_startup_guards import scrub
    try:
        text = "%s: %s" % (type(error).__name__, error)
    except Exception:
        return
    line = scrub(" ".join(text.split()))[:1024]
    sys.stderr.write("sglang refused the server arguments: " + line + "\n")
    sys.stderr.flush()


def construct_server_args(spec, placement, guarded_constructor, available_bytes=None,
                          approvals=None):
    """Map and check a caller-provided, already guarded ServerArgs class.

    This does not import or authorize the constructor. It may have side effects,
    even when construction fails; callers must not automatically retry it.
    Keywords merge as: safe defaults, typed fields, extra arguments, then the
    reserved subset, which always wins. After resolve_once every reserved field
    must keep its exact value AND type (closed effective_args_mismatch).
    """
    public = _validated_public(spec)
    _check_placement(public, placement)
    settings = public["settings"]
    if approvals is None:
        approvals = extra_args_policy.parse_approvals(None)
    # ADR 0014 §8 / T21: trust_remote_code runs checkpoint code; the host's
    # approval is rechecked here, not only at deploy time.
    if settings["trust_remote_code"] is True and approvals.trust_remote_code is not True:
        raise ServerArgsError("sensitive_option_refused")
    extra = parse_extra_args(settings["extra_args"], guarded_constructor, approvals,
                             spec._checkpoint_root)
    # ADR 0024: resolution never passes a chosen parser beside the same extra.
    if any(name in settings and name in extra
           for name in ("tool_call_parser", "reasoning_parser")):
        raise ServerArgsError("effective_args_mismatch")
    # ADR 0014 amendment A14: extras that size the state pool own it, and
    # capyctl then sizes none (resolution prevents both; this rechecks).
    if "max_mamba_cache_size" in settings and (
            "max_mamba_cache_size" in extra or "mamba_full_memory_ratio" in extra):
        raise ServerArgsError("effective_args_mismatch")
    available = (available_bytes_for(settings["memory"]) if available_bytes is None
                 else available_bytes)
    fraction = static_fraction(
        fraction_bytes(settings["memory"], available), available)
    reserved = dict(_RESERVED_CONSTANT)
    if os.environ.get("CAPYCTL_DEBUG_ENGINE_LOGS") == "1":
        reserved.update(log_level="debug", log_level_http="debug")
    reserved.update(model_path=spec._checkpoint_root, tokenizer_path=spec._checkpoint_root,
                    served_model_name=public["served_name"],
                    api_key=spec._inference_key, admin_api_key=spec._admin_key,
                    host="127.0.0.1", port=int(public["endpoint"].rsplit(":", 1)[1]),
                    base_gpu_id=placement.cuda_index, mem_fraction_static=fraction,
                    # ADR 0010: residency decides the park strategy at launch.
                    # ADR 0028 §12 (R12): on every rank of a group alike.
                    enable_memory_saver=settings["memory_saver"],
                    enable_weights_cpu_backup=settings["cpu_weight_backup"])
    # ADR 0028 §10: a group member's parallelism and rendezvous come from its
    # descriptor's group, never the single-rank constants. On a worker the key
    # fields stay None (it is handed no credential, ADR 0012) and its health
    # server binds the loopback worker port the descriptor's endpoint names.
    group = settings.get("group")
    if group is not None:
        reserved.update({name: group[name] for name in GROUP_FIELDS})
    keywords = dict(_SAFE_DEFAULTS)
    keywords.update(typed_keywords(settings))
    keywords.update(extra)
    keywords.update(reserved)
    # trust_remote_code is host-approved (ADR 0014 §8); it is checked with the
    # reserved subset so no resolution path can turn it on unapproved.
    expected = dict(reserved, trust_remote_code=settings["trust_remote_code"])
    try:
        native = guarded_constructor(**keywords)
        native.resolve_once()
    except Exception as error:
        _log_refusal(error)
        raise ServerArgsError("server_args_construction_failed") from None
    checked = CheckedServerArgs(native, tuple(expected.items()),
                                settings["cuda_graphs"] is False,
                                GROUP_FIELDS if group is not None else ())
    checked.revalidate()
    return checked
