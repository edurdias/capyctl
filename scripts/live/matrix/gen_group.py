#!/usr/bin/env python3
"""Multi-node group fixtures for rows MN1-MN8 (ADR 0028, design section 17.2).

One fixture per engine, a two-rank tensor-parallel group with host A as the head
(rank 0, serving the API) and host B as the worker (rank 1):

  vg-30   vLLM, Qwen3-30B-A3B, residency deep
  sg-30   SGLang, the same checkpoint, no residency (resolves deep: SGLang groups
          park again, ADR 0028 note of 2026-10-16)
  tg-n35  TensorFold, Nemotron 3.5 Lightning 30B-A3B MLX 4-bit, restart_only
          (TensorFold has no deep park), its memory stated in `resources`

The fixture's name is its route. The model is --model-dir, a directory under the
model store (grouplib.sh passes GROUP_MODEL_VLLM_SGLANG or GROUP_MODEL_TENSORFOLD),
the same path on both hosts (SGLang deep groups need that, ADR 0028 §6);
each host measures its own copy's digest and the server refuses unequal ones
(`group_checkpoint_mismatch`). The engine settings come from models.json's q30
entry; the memory request is per member, since each host charges its own rank.

usage: gen_group.py --engine vllm|sglang|tensorfold --model-dir DIR --out-dir DIR
                    [--request SIZE] [--kv-cache SIZE] [--variant TAG] [--document-json JSON]

--variant writes <name>.<TAG>.yaml for a deployment named and routed <name>-<TAG>;
--document-json merges fields into the document (null removes), as gen_deployment.py.
"""

import argparse
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from gen_deployment import merge  # noqa: E402  (also checks HOST_A, HOST_B, MODELS_ROOT)

NAMES = {"vllm": "vg-30", "sglang": "sg-30", "tensorfold": "tg-n35"}
# Per member: the q30 checkpoint is about 61 GB in bfloat16, so each rank holds
# about half of the weights plus its KV cache.
REQUEST = "48GiB"
KV_CACHE = "8GiB"
# TensorFold states its memory (ADR 0023 §4): what one rank holds on the GPU and
# in host RAM. The declared Ready allocation also caps TensorFold's CUDA startup
# budget (TENSORFOLD_CUDA_MEMORY_LIMIT_GB), and at TP 2 Nemotron loads the whole
# model before its split: live 2026-10-15 a rank estimated 32.49 GiB at startup, so
# 30GiB + 2GiB refused every window. MN_TENSORFOLD_GPU overrides the GPU figure.
TENSORFOLD_RESOURCES = {"gpu": os.environ.get("MN_TENSORFOLD_GPU", "40GiB"), "ram": "2GiB"}


def env(name):
    value = os.environ.get(name)
    if not value:
        sys.exit(f"{name} is not set: run through the matrix scripts, which source hosts.local.env")
    return value


def document(engine, args):
    models_root = env("MODELS_ROOT").rstrip("/")
    hosts = [env("HOST_A"), env("HOST_B")]
    name = NAMES[engine]
    doc = {
        "schema_version": 1,
        "kind": "deployment",
        "name": name,
        "routes": [name],
        "runtime_profile": engine,
        "runtime_profile_revision": 1,
        "recipe": "standalone",
        "recovery": "reconcile",
        # ADR 0028 §2: named hosts only, head first; one rank per host.
        "topology": {"tensor_parallel": 2, "pipeline_parallel": 1},
        "placement": {"hosts": hosts},
        "devices": [{"id": "gpu0", "sharing": "shared"}],
    }
    if engine == "tensorfold":
        doc["residency"] = "restart_only"
        doc["resources"] = dict(TENSORFOLD_RESOURCES)
        doc["engine_config"] = {"context_length": 32768}
        # The request deadline and initialize timeout are left to TensorFold's
        # derived first-build bound (ADR 0023 §4).
    else:
        with open(os.path.join(HERE, "models.json")) as handle:
            spec = json.load(handle)["models"]["30"]
        config = {
            "dtype": spec["dtype"],
            "context_length": spec["context_length"],
            "max_concurrent_requests": spec["max_concurrent_requests"],
            "cuda_graphs": spec["cuda_graphs"],
            "memory": {"request": args.request, "kv_cache": args.kv_cache},
        }
        extra = (spec.get("extra_args") or {}).get(engine)
        if extra:
            config["accept_extra_args"] = True
            config["extra_args"] = list(extra)
        if engine == "vllm":
            doc["residency"] = "deep"
        doc["engine_config"] = config
        doc["request_deadline"] = "1800s"
        # Two hosts load and rendezvous before the head answers.
        doc["timeouts"] = {"initialize": "1800s"}
    directory = args.model_dir
    doc["model"] = {
        "source": {"type": "local", "path": f"{models_root}/{directory}"},
        # The placeholder, as gen_deployment.py: each host measures the real digest.
        "content_fingerprint": f"sha256:{directory}",
        "revision": "r1",
    }
    return doc


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--engine", required=True, choices=sorted(NAMES))
    parser.add_argument("--model-dir", required=True)
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--request", default=REQUEST)
    parser.add_argument("--kv-cache", default=KV_CACHE)
    parser.add_argument("--variant")
    parser.add_argument("--document-json")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", args.model_dir) or args.model_dir in (".", ".."):
        parser.error("--model-dir is one directory name under the model store")
    if args.variant and not re.fullmatch(r"[a-z0-9]+", args.variant):
        parser.error("--variant must be lowercase letters and digits")
    doc = document(args.engine, args)
    if args.document_json:
        merge(doc, json.loads(args.document_json))
    name = NAMES[args.engine]
    if args.variant:
        doc["name"] = f"{name}-{args.variant}"
        doc["routes"] = [doc["name"]]
    os.makedirs(args.out_dir, exist_ok=True)
    path = os.path.join(args.out_dir, f"{name}.{args.variant}.yaml" if args.variant else f"{name}.yaml")
    with open(path, "w") as handle:
        json.dump(doc, handle, indent=1)
        handle.write("\n")
    print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
