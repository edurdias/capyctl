#!/usr/bin/env python3
"""Deployment fixtures for the matrix, one per model x engine x host (plan unit W2).

Names follow matrix section 3: <engine><host>-<model>, engine v (vLLM) or s
(SGLang), host a (HOST_A) or b (HOST_B), model 4, 14, 27, 27f (the
NVFP4 anchor with an FP8 KV cache) or 30. A fixture's route is its name unless
--route gives a shared replica route.

Every fixture carries a typed engine_config (ADR 0014 section 2) with a declared
memory request and KV cache (P2); `resources:` is omitted so the phases derive
from the request. The anchor sets language_model_only and the per-engine
quantization from models.json. content_fingerprint is the checkpoint payload
digest when a checkpoints.json from `e0.sh checkpoints` is given.

usage: gen_deployment.py --hosts-json FILE --out-dir DIR [--measured FILE]
                         [--checkpoints FILE] [--residency deep|restart_only]
                         [--route ROUTE] [--request-deadline 900s] (--all | NAME ...)
                         [--engine-config-json JSON --variant TAG] [--co]
                         [--document-json JSON]

--engine-config-json merges typed engine_config fields into each named fixture
(a null value removes the field); with --variant the file is written as
<name>.<TAG>.yaml for a new deployment named and routed <name>-<TAG>. The CLI
has no revision verb yet, so a variant or a rerun is a new
deployment (M16 engine-recipe variants).

--document-json (2026-09-23) merges fields into the whole deployment document
the same way, after --engine-config-json. It is how a row states what the typed
generator has no flag for: `timeouts.initialize` (M74), or a model source and
content_fingerprint for a failure shape (M38). Use it with --variant.

--co (2026-09-23) uses each model's co-residence variant from models.json
(context 8192, smaller KV cache and request, sized so q30+q4 and q27f+q14+q4
fit the normal budget together; `gen_budgets.py check` proves the fit) and
writes <name>.co.yaml for a deployment named and routed <name>-co unless
--variant names another tag. A variant's `startup_bytes`, when set, becomes
engine_config.memory.startup; otherwise the store's placeholder startup peak
applies until a first run measures it.
"""

import argparse
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from gen_budgets import load_models  # noqa: E402

# Model keys come from models.json (2026-09-25: the single-box benchmark adds
# mc2, q36, ling, g2, 27b and their drafter variants).
NAME = re.compile(r"^(?P<engine>[vs])(?P<host>a|b)-(?P<model>[a-z0-9]+)$")
# Host names and the models root come from the harness environment (lib.sh,
# from hosts.local.env), so no lab-specific name or path lives here.
def _env(name):
    value = os.environ.get(name)
    if not value:
        sys.exit(f"{name} is not set: run through the matrix scripts, which source hosts.local.env")
    return value


HOSTS = {"a": _env("HOST_A"), "b": _env("HOST_B")}
ENGINES = {"v": "vllm", "s": "sglang"}
MODELS_ROOT = _env("MODELS_ROOT")
ULID = re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")


def host_ids(path):
    with open(path) as handle:
        listing = json.load(handle)
    hosts = listing.get("hosts", listing) if isinstance(listing, dict) else listing
    ids = {}
    for entry in hosts:
        name = entry.get("name") or entry.get("host_name")
        ident = entry.get("host_id") or entry.get("id")
        if name and ident:
            ids[name] = ident
    return ids


def fixture(name, args, models, ids, checkpoints):
    match = NAME.match(name)
    if not match:
        sys.exit(f"{name!r} is not <v|s><a|b>-<model key>")
    engine = ENGINES[match["engine"]]
    host = HOSTS[match["host"]]
    key = match["model"]
    if key not in models:
        sys.exit(f"models.json has no model {key!r}")
    spec = models[key]
    if args.co:
        if "co" not in spec:
            sys.exit(f"models.json has no co variant for model {key}")
        spec = {k: v for k, v in spec.items() if k != "co"}
        spec.update(models[key]["co"])
    if host not in ids:
        sys.exit(f"{host} is not in the hosts listing; enroll it first")
    if not ULID.match(ids[host]) and not ids[host].startswith("01DRYRUN"):
        sys.exit(f"host id {ids[host]!r} is not a ULID")
    # A model may pin its residency (2026-09-25 benchmark: restart_only, since
    # SGLang refuses deep parking for modelopt checkpoints and parking is not measured).
    residency = spec.get("residency") or args.residency
    if engine == "sglang" and residency != "deep":
        # SGLang's protected entry refuses a launch shape without the memory
        # saver, which only a deep residency derives (found live, standalone template).
        print(f"warning: {name}: SGLang with residency {residency} is refused at launch", file=sys.stderr)
    config = {
        "dtype": spec["dtype"],
        "context_length": spec["context_length"],
        "max_concurrent_requests": spec["max_concurrent_requests"],
        "cuda_graphs": spec["cuda_graphs"],
        "memory": {"request": f"{spec['request_bytes']}B", "kv_cache": f"{spec['kv_cache_bytes']}B"},
    }
    if spec.get("startup_bytes"):
        config["memory"]["startup"] = f"{spec['startup_bytes']}B"
    for field in ("quantization", "kv_cache_dtype"):
        value = spec.get(field)
        if isinstance(value, dict):
            value = value.get(engine)
        if value is not None:
            config[field] = value
    # M16: language_model_only and extra_args may be per engine family
    # (SGLang 0.5.20 refuses --language-model-only for the 27B family; vLLM
    # on q30 needs --moe-backend triton).
    language_model_only = spec.get("language_model_only")
    if isinstance(language_model_only, dict):
        language_model_only = language_model_only.get(engine)
    if language_model_only:
        config["language_model_only"] = True
    extra_args = spec.get("extra_args")
    if isinstance(extra_args, dict):
        extra_args = extra_args.get(engine)
    if extra_args:
        config["accept_extra_args"] = True
        # Drafter checkpoints are named relative to the host's model store.
        config["extra_args"] = [a.replace("@MODELS_ROOT@", MODELS_ROOT) for a in extra_args]
    digest = None
    if checkpoints:
        per_host = checkpoints.get(host, {})
        digest = per_host.get(spec["dir"])
        if digest is None:
            sys.exit(f"checkpoints.json has no digest for {spec['dir']} on {host}")
    # ADR 0008 amendment 2026-09-24: a model with `hf` declares a pinned Hugging
    # Face source; the host materializes it under <model store>/sources/ before
    # the first placement (the host document must allow huggingface sources).
    if spec.get("hf"):
        source = {"type": "huggingface", "repo": spec["hf"]["repo"], "revision": spec["hf"]["revision"]}
    else:
        source = {"type": "local", "path": f"{MODELS_ROOT}/{spec['dir']}"}
    return {
        "schema_version": 1,
        "kind": "deployment",
        "name": name,
        "host": ids[host],
        "routes": [args.route or name],
        "runtime_profile": engine,
        "runtime_profile_revision": 1,
        "recipe": "standalone",
        "residency": residency,
        "recovery": "reconcile",
        "request_deadline": args.request_deadline,
        "model": {
            "source": source,
            "content_fingerprint": f"sha256:{digest}" if digest else f"sha256:{spec['dir']}",
            "revision": "r1",
        },
        "devices": [{"id": "gpu0", "sharing": "shared"}],
        "engine_config": config,
    }


def merge(target, update):
    for key, value in update.items():
        if value is None:
            target.pop(key, None)
        elif isinstance(value, dict) and isinstance(target.get(key), dict):
            merge(target[key], value)
        else:
            target[key] = value


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("names", nargs="*")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--hosts-json", required=True)
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--measured")
    parser.add_argument("--checkpoints")
    parser.add_argument("--residency", default="deep", choices=["deep", "restart_only"])
    parser.add_argument("--route")
    parser.add_argument("--request-deadline", default="900s")
    parser.add_argument("--engine-config-json")
    parser.add_argument("--variant")
    parser.add_argument("--co", action="store_true", help="the co-residence variant of each model")
    parser.add_argument("--document-json", help="fields merged into the whole document (null removes)")
    args = parser.parse_args()
    if args.co and not args.variant:
        args.variant = "co"
    if args.variant and not re.fullmatch(r"[a-z0-9]+", args.variant):
        parser.error("--variant must be lowercase letters and digits")
    names = list(args.names)
    if args.all:
        names += [f"{e}{h}-{m}" for e in "vs" for h in ("a", "b") for m in ("4", "14", "27", "27f", "30")]
    if not names:
        parser.error("name fixtures or pass --all")
    models = load_models(args.measured)
    ids = host_ids(args.hosts_json)
    checkpoints = None
    if args.checkpoints:
        with open(args.checkpoints) as handle:
            checkpoints = json.load(handle)
    os.makedirs(args.out_dir, exist_ok=True)
    for name in names:
        document = fixture(name, args, models, ids, checkpoints)
        if args.engine_config_json:
            merge(document["engine_config"], json.loads(args.engine_config_json))
        if args.document_json:
            merge(document, json.loads(args.document_json))
        if args.variant:
            document["name"] = f"{name}-{args.variant}"
            if not args.route:
                document["routes"] = [document["name"]]
        path = os.path.join(args.out_dir, f"{name}.{args.variant}.yaml" if args.variant else f"{name}.yaml")
        with open(path, "w") as handle:
            json.dump(document, handle, indent=1)
            handle.write("\n")
        print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
