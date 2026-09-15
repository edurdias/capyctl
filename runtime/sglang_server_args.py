"""Pure closed-recipe mapping for SGLang fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1.

No SGLang imports, device access, or launch authority live here. In particular,
ServerArgs.__post_init__ is NOT pure: it calls platform plugin hooks, reads model
configuration, queries GPU capacity, and propagates environment variables. A
production caller must establish source/package, logging, plugin, environment,
checkpoint, and physical-placement guards BEFORE importing/calling its constructor.
The entrypoint deliberately does not call this module yet.

Checks cover the explicitly mapped closed-recipe fields and resolved graph phases;
they do not attest unlisted backend defaults, compiled kernels, complete worker
enrollment, the actual saver implementation, or observed allocation bounds.
Those independent checks remain mandatory. Scalar limits are not byte guarantees.
"""

from dataclasses import dataclass, field
import json

from . import sglang_entry


class ServerArgsError(Exception):
    """Sanitized boundary error; never render native arguments or exceptions."""

    def __init__(self, code):
        if code not in ("invalid_launch_inputs", "placement_mismatch",
                        "server_args_construction_failed", "effective_args_mismatch"):
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


# Keyword names and meanings inspected in pinned python/sglang/srt/server_args.py.
# All values are immutable scalars. No mapping is inferred from CLI flag aliases.
# Defaults below are explicit exclusions, not evidence that the features are absent
# after import; platform/plugins and post-init can mutate native objects.
_FIXED = {
    "tokenizer_mode": "auto", "tokenizer_backend": "huggingface",
    "skip_tokenizer_init": False, "tokenizer_worker_num": 1,
    "detokenizer_worker_num": 1, "load_format": "auto",
    "model_loader_extra_config": "{}", "model_impl": "sglang",
    "model_config_parser": "hf", "json_model_override_args": "{}",
    "trust_remote_code": False, "is_embedding": False, "enable_multimodal": False,
    "dtype": "bfloat16", "kv_cache_dtype": "bfloat16",
    # Pinned post-init turns unquant into None while recording the opt-out.
    "quantization": "unquant", "quantization_param_path": None,
    "modelopt_quant": None, "quantize_and_serve": False,
    "modelopt_checkpoint_restore_path": None, "modelopt_checkpoint_save_path": None,
    "modelopt_export_path": None, "rl_quant_profile": None,
    "context_length": 4096, "max_running_requests": 8, "max_total_tokens": 4096,
    "tp_size": 1, "dp_size": 1, "pp_size": 1, "ep_size": 1,
    "dcp_size": 1, "attn_cp_size": 1, "moe_dp_size": 1,
    "nnodes": 1, "node_rank": 0, "device": "cuda", "gpu_id_step": 1,
    "use_ray": False, "dist_init_addr": None,
    "enable_prefill_cp": False, "enable_dp_attention": False,
    "enable_dp_lm_head": False, "enable_eplb": False,
    "enable_elastic_expert_backup": False,
    "disable_prefill_cuda_graph": True, "disable_decode_cuda_graph": True,
    "cuda_graph_backend_prefill": "disabled", "cuda_graph_backend_decode": "disabled",
    "enable_torch_compile": False, "enable_torch_compile_debug_mode": False,
    "torchao_config": "", "enable_memory_saver": True,
    "enable_weights_cpu_backup": False, "enable_draft_weights_cpu_backup": False,
    "speculative_algorithm": None, "speculative_draft_model_path": None,
    "enable_lora": False, "lora_paths": None,
    "disaggregation_mode": "null", "disaggregation_decode_enable_offload_kvcache": False,
    "encoder_only": False, "language_only": False, "enable_pdmux": False,
    "enable_hierarchical_cache": False, "hicache_storage_backend": None,
    "hicache_storage_backend_extra_config": None, "enable_hisparse": False,
    "enable_lmcache": False, "lmcache_config_file": None,
    "enable_flexkv": False, "flexkv_config_file": None, "radix_cache_backend": None,
    "cpu_offload_gb": 0, "disable_radix_cache": False,
    "enable_page_major_kv_layout": False, "enable_unified_memory": False,
    "grpc_port": None, "grpc_mode": False, "smg_grpc_mode": False,
    "sidecar": None, "sidecar_args": None, "smg_http_sidecar_port": None,
    "fastapi_root_path": "", "enable_http2": False,
    "ssl_keyfile": None, "ssl_certfile": None, "ssl_ca_certs": None,
    "ssl_keyfile_password": None, "enable_ssl_refresh": False,
    "skip_server_warmup": True, "warmups": None,
    # These levels suppress ordinary argument info logs, NOT a complete logging
    # guard: native code formats f"{server_args=}" before logger filtering.
    "log_level": "error", "log_level_http": "error", "log_requests": False,
    "log_requests_target": None, "crash_dump_folder": None,
    "enable_request_time_stats_logging": False, "enable_trace": False,
    "enable_metrics": False, "enable_custom_logit_processor": False,
}


def _validated_public(spec):
    try:
        if type(spec) is not sglang_entry.LaunchSpec:
            raise ValueError()
        public = json.loads(spec._public_json)
        private = json.dumps({"schema_version": 1,
                              "kind": "sglang_candidate_private_launch",
                              "checkpoint_root": spec._checkpoint_root,
                              "public_settings": public}).encode()
        payloads = {3: private, 4: spec._inference_key.encode("ascii"),
                    5: spec._admin_key.encode("ascii")}
        # Reuse the entire strict boundary: LaunchSpec's Python constructor is
        # not itself a validation capability. No descriptor I/O occurs here.
        sglang_entry.build_launch(
            ["--public-settings-json", spec._public_json,
             "--launch-descriptor-fd", "3", "--inference-credential-fd", "4",
             "--admin-credential-fd", "5"], payloads.__getitem__)
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


@dataclass(frozen=True, repr=False)
class CheckedServerArgs:
    """Private native object plus retained explicit expectations, not authority.

    The guarded launcher may consume _native only after revalidate and all its
    independent checks. Never log _native: upstream dataclass repr includes keys.
    Revalidation does not independently recollect device or process observations.
    """

    _native: object = field(repr=False)
    _expected: tuple = field(repr=False)

    def __repr__(self):
        return "CheckedServerArgs(<private; not launch authority>)"

    def revalidate(self):
        try:
            for key, expected in self._expected:
                value = getattr(self._native, key)
                if type(value) is not type(expected) or value != expected:
                    raise ValueError()
            config = self._native.cuda_graph_config
            for phase in (config.prefill, config.decode):
                if type(phase.backend) is not str or phase.backend != "disabled":
                    raise ValueError()
        except Exception:
            raise ServerArgsError("effective_args_mismatch") from None


def construct_server_args(spec, placement, guarded_constructor):
    """Map and check a caller-provided, already guarded pinned constructor.

    This does not import or authorize the constructor. It may have side effects,
    even when construction fails; callers must not automatically retry it.
    Only the narrow documented post-init normalization (unquant -> None) is
    accepted. Every other explicit field must retain its exact value AND type.
    Resolved graph backends must also remain disabled.
    """
    public = _validated_public(spec)
    _check_placement(public, placement)
    keywords = dict(_FIXED)
    keywords.update(model_path=spec._checkpoint_root, tokenizer_path=spec._checkpoint_root,
                    served_model_name=public["served_name"], revision=public["checkpoint_revision"],
                    api_key=spec._inference_key, admin_api_key=spec._admin_key,
                    host="127.0.0.1", port=int(public["endpoint"].rsplit(":", 1)[1]),
                    base_gpu_id=placement.cuda_index,
                    mem_fraction_static=(public["settings"]["requested_budget"]
                                         ["static_memory_fraction_bps"] / 10000))
    expected = dict(keywords)
    expected["quantization"] = None
    expected["_quantization_explicitly_unset"] = True
    try:
        native = guarded_constructor(**keywords)
    except Exception:
        raise ServerArgsError("server_args_construction_failed") from None
    checked = CheckedServerArgs(native, tuple(expected.items()))
    checked.revalidate()
    return checked
