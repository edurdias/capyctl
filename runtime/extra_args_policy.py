"""Launch-time gate on deployment extra arguments (ADR 0014 §8, SPEC §8.2).

The deploy-time check (`crates/mllm-config/src/engine_policy.rs`) sees only the
spelling a deployment wrote. The engines' parsers expand any unambiguous
abbreviation and accept aliases, so `--engine-info` reaches SGLang's
`engine_info_bootstrap_port` and `--master-ad` reaches vLLM's `master_addr`.
This gate therefore decides on the destination the installed parser resolved:

- code-loading (a class, loader or plugin), listener or egress, path and JSON
  configuration destinations need the host's named approval
  (`security.approved_options`);
- an approved path destination must name paths inside `security.approved_paths`
  (a checkpoint-exempt one may instead lie inside the checkpoint);
- anything else is ordinary and passes.

The host's approvals reach the entry as one closed JSON document in the
`MLLM_EXTRA_APPROVALS` environment variable, rendered by the adapter from the
approved profile. Its absence approves nothing. Standard library only; no
engine import. Refusals name the destination class, never a value.
"""

import json
import os

ENV = "MLLM_EXTRA_APPROVALS"

CODE = "code"
PATH = "path"
PATH_EXEMPT = "path_checkpoint_exempt"
EGRESS = "listener_or_egress"
CONFIG = "config"

# The same shapes the deploy-time check applies to option spellings, here on
# parsed destinations (ADR 0014 open issue 5).
_LISTENER = ("_port", "_ports", "_host", "_address", "_addr", "_ip", "_socket",
             "_endpoint", "_endpoints", "_url", "_urls", "_token", "_bind")
_PATH = ("_path", "_paths", "_dir", "_directory", "_folder", "_file")
_CONFIG = ("_config",)
# Code loading: a class, a loader or a plugin named by the argument.
_CODE = ("_cls", "_class", "_loader")

# ADR 0014 §8: the explicit lists, as destinations (engine_policy.rs
# VLLM_SENSITIVE / SGLANG_SENSITIVE).
_EXPLICIT = {
    "vllm": {
        "worker_cls": CODE, "worker_extension_cls": CODE, "logits_processors": CODE,
        "logits_processor_pattern": CODE, "tool_parser_plugin": CODE,
        "reasoning_parser_plugin": CODE,
        "download_dir": PATH, "tokenizer": PATH_EXEMPT, "chat_template": PATH_EXEMPT,
        "lora_modules": PATH, "speculative_config": PATH, "generation_config": PATH,
        "allowed_local_media_path": PATH, "hf_config_path": PATH,
        "otlp_traces_endpoint": EGRESS, "kv_transfer_config": EGRESS,
        "kv_events_config": EGRESS, "load_format": EGRESS,
        "allowed_media_domains": EGRESS, "hf_token": EGRESS,
    },
    "sglang": {
        "enable_custom_logit_processor": CODE,
        "download_dir": PATH, "chat_template": PATH_EXEMPT,
        "completion_template": PATH_EXEMPT, "lora_paths": PATH,
        "speculative_draft_model_path": PATH, "file_storage_path": PATH,
        "otlp_traces_endpoint": EGRESS, "tool_server": EGRESS,
        "kv_events_config": EGRESS, "load_format": EGRESS,
        "remote_instance_weight_loader_seed_instance_ip": EGRESS,
    },
}


class Refused(Exception):
    """Closed refusal; carries a category, never an argument value."""

    def __init__(self, code="sensitive_option_refused"):
        super().__init__(code)
        self.code = code


class Approvals(tuple):
    """(options as destinations, approved directories, trust_remote_code)."""

    def __new__(cls, options, paths, trust_remote_code):
        return super().__new__(cls, (tuple(options), tuple(paths), trust_remote_code))

    @property
    def options(self):
        return self[0]

    @property
    def paths(self):
        return self[1]

    @property
    def trust_remote_code(self):
        return self[2]


def _destination(option):
    if (type(option) is not str or not option.startswith("--") or len(option) <= 2
            or "=" in option or len(option) > 256):
        raise Refused("invalid_extra_approvals")
    return option[2:].lower().replace("-", "_")


def _normal_directory(path):
    if type(path) is not str or not path.startswith("/") or len(path) > 4096:
        raise Refused("invalid_extra_approvals")
    if any(part in (".", "..") for part in path.split("/")):
        raise Refused("invalid_extra_approvals")
    return path


def parse_approvals(raw):
    """The host's approvals, strictly; `None` (no document) approves nothing."""
    if raw is None:
        return Approvals((), (), False)
    try:
        value = json.loads(raw)
    except (ValueError, TypeError):
        raise Refused("invalid_extra_approvals") from None
    if (type(value) is not dict
            or set(value) != {"options", "paths", "trust_remote_code"}
            or type(value["options"]) is not list or type(value["paths"]) is not list
            or type(value["trust_remote_code"]) is not bool
            or len(value["options"]) > 256 or len(value["paths"]) > 256):
        raise Refused("invalid_extra_approvals")
    return Approvals((_destination(option) for option in value["options"]),
                     (_normal_directory(path) for path in value["paths"]),
                     value["trust_remote_code"])


def approvals_from_environment(environ=None):
    """Read and remove the approvals document, so no engine child inherits it."""
    environ = os.environ if environ is None else environ
    return parse_approvals(environ.pop(ENV, None))


def classify(engine, dest):
    """Why a parsed destination needs approval, or None when it is ordinary."""
    kind = _EXPLICIT.get(engine, {}).get(dest)
    if kind is not None:
        return kind
    parts = dest.split("_")
    if dest.endswith(_CODE) or "plugin" in parts or "plugins" in parts:
        return CODE
    if dest.endswith(_LISTENER) or "bind" in parts or dest.startswith("remote_instance"):
        return EGRESS
    if dest.endswith(_PATH):
        return PATH
    if dest.endswith(_CONFIG):
        return CONFIG
    return None


def _lexically_within(value, root):
    if not value.startswith("/") or not root.startswith("/"):
        return False
    parts = value.split("/")[1:]
    if any(part in ("", ".", "..") for part in parts):
        return False
    root_parts = [part for part in root.split("/")[1:] if part]
    return parts[:len(root_parts)] == root_parts


def _within(value, root):
    """Containment, lexically (engine_policy.rs `path_within`) and resolved.

    SPEC §8.2 / T21: at launch the path is also resolved through every symlink
    that exists now, so a link inside an approved directory cannot lead out of
    it. What does not exist yet resolves lexically below its deepest existing
    ancestor, which must itself stay inside.
    """
    if type(value) is not str or type(root) is not str:
        return False
    if not _lexically_within(value, root):
        return False
    return _lexically_within(os.path.realpath(value), os.path.realpath(root))


def _paths_of(value):
    if type(value) is str:
        return [value]
    if type(value) in (list, tuple) and value and all(type(v) is str for v in value):
        return list(value)
    # A structured value (a JSON object, a parsed record) names paths this
    # check cannot see: it is never admitted as a path.
    raise Refused()


def check(engine, supplied, approvals, checkpoint):
    """Refuse any supplied destination the host has not approved.

    `supplied` maps each destination the deployment's extra arguments set, as
    the installed parser resolved it, to its parsed value.
    """
    for dest, value in supplied.items():
        kind = classify(engine, dest)
        if kind is None:
            continue
        if kind == PATH_EXEMPT and checkpoint is not None:
            try:
                # The checkpoint's own files are digest-verified (ADR 0014 §7)
                # and may link into the model store (the Hugging Face snapshot
                # layout), so containment here is lexical.
                if all(type(path) is str and _lexically_within(path, checkpoint)
                       for path in _paths_of(value)):
                    continue
            except Refused:
                pass
        if dest not in approvals.options:
            raise Refused()
        if kind in (PATH, PATH_EXEMPT):
            for path in _paths_of(value):
                if not any(_within(path, root) for root in approvals.paths):
                    raise Refused()
