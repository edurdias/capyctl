#!/usr/bin/env python3
"""Generate an approved host document from templates/host.json (plan unit W2).

Inputs are measured on the host, never typed by hand: the device observation
(`python3 -m runtime.sglang_device` on the host, which prints host_id, digest
and the physical GPU UUID) and each engine's reported version. The budget comes
from budgets/<policy>.yaml (D10). Profiles carry only host-fixed fields; engine
tuning lives in each deployment's engine_config (ADR 0014 section 1).

usage: gen_host_doc.py --device-json FILE --ip IP --run-root DIR --policy normal|tight
                       --sglang-version V --vllm-version V --vllm-venv DIR
                       [--sglang-venv DIR] [--remote-tree DIR] [--models-root DIR]
                       [--ingress-port 9443] [--managed-runtime] [--no-profiles]
                       [--hf-max-bytes SIZE] [--approve-speculation] [--cuda-home DIR] --out FILE

--hf-max-bytes and --approve-speculation (2026-09-25, single-box benchmark)
opt the host into Hugging Face model sources with that store ceiling (ADR 0008
amendment) and approve the speculative-decoding options (vLLM
`--speculative-config` and SGLang `--speculative-draft-model-path`, with draft
models inside the model store; ADR 0014 section 8). --cuda-home sets every
profile's `cuda_home` (SPEC section 13.3 as amended 2026-09-25: `<dir>/bin` joins
the engine PATH; vLLM needs `nvcc` there for FlashInfer).
"""

import argparse
import ipaddress
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
UUID = re.compile(r"^GPU-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
DIGEST = re.compile(r"^[0-9a-f]{64}$")
NAME = re.compile(r"^[A-Za-z0-9_.-]{1,128}$")
VERSION = re.compile(r"^[0-9A-Za-z.+_-]{1,64}$")


def substitute(node, values):
    if isinstance(node, dict):
        return {key: substitute(value, values) for key, value in node.items()}
    if isinstance(node, list):
        return [substitute(value, values) for value in node]
    if isinstance(node, str):
        if node in values and not isinstance(values[node], str):
            return values[node]  # typed placeholder (an integer)
        for key, value in values.items():
            if isinstance(value, str):
                node = node.replace(key, value)
        if "@" in node and re.search(r"@[A-Z_]+@", node):
            sys.exit(f"unfilled placeholder in {node!r}")
        return node
    return node


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--device-json", required=True)
    parser.add_argument("--ip", required=True)
    parser.add_argument("--run-root", required=True)
    parser.add_argument("--policy", required=True, choices=["normal", "tight"])
    parser.add_argument("--sglang-version", required=True)
    parser.add_argument("--vllm-version", required=True)
    parser.add_argument("--vllm-venv", required=True)
    parser.add_argument("--sglang-venv", required=True)
    parser.add_argument("--remote-tree", required=True)
    parser.add_argument("--models-root", required=True)
    parser.add_argument("--ingress-port", type=int, default=9443)
    # Release validation: leave runtime_dir out, so the host uses the managed
    # runtime its binary writes to <state_dir>/runtime (docs/operations/install.md).
    parser.add_argument("--managed-runtime", action="store_true")
    # ADR 0018 (rows ENG1, ENG4): a bare host document for `engine add` rows,
    # so every published profile in the run comes only from `engines.yaml`.
    parser.add_argument("--no-profiles", action="store_true")
    parser.add_argument("--hf-max-bytes")
    parser.add_argument("--approve-speculation", action="store_true")
    parser.add_argument("--cuda-home")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    with open(args.device_json) as handle:
        device = json.load(handle)
    if device.get("schema") != "mllm-nvidia-inventory-v1":
        sys.exit("device observation has an unexpected schema")
    host = device["host_id"]
    devices = device.get("devices") or []
    # Host authorization accepts exactly one device in one unified domain.
    if len(devices) != 1:
        sys.exit(f"expected exactly one GPU, observed {len(devices)}")
    uuid = devices[0]["physical_gpu_uuid"]
    checks = [(NAME, host, "host name"), (UUID, uuid, "GPU UUID"), (DIGEST, device["digest"], "digest"),
              (VERSION, args.sglang_version, "SGLang version"), (VERSION, args.vllm_version, "vLLM version")]
    for pattern, value, label in checks:
        if not pattern.match(value):
            sys.exit(f"{label} {value!r} is malformed")
    ip = ipaddress.ip_address(args.ip)
    # Private ingress is cleartext and allowed only on loopback or Tailscale 100.64/10.
    if not (ip.is_loopback or ip in ipaddress.ip_network("100.64.0.0/10")):
        sys.exit(f"ingress address {ip} is not loopback or 100.64/10")
    if not 1 <= args.ingress_port <= 65535 or 8100 <= args.ingress_port <= 8199:
        sys.exit("ingress port must be outside the engine port range 8100-8199")
    for path in (args.run_root, args.vllm_venv, args.sglang_venv, args.remote_tree, args.models_root):
        if not os.path.isabs(path) or "/./" in path or "/../" in path or "//" in path:
            sys.exit(f"path {path!r} must be absolute and normalized")

    with open(os.path.join(HERE, "budgets", args.policy + ".yaml")) as handle:
        budget = json.load(handle)
    with open(os.path.join(HERE, "templates", "host.json")) as handle:
        template = json.load(handle)
    values = {
        "@HOST_NAME@": host,
        "@RUN_ROOT@": args.run_root.rstrip("/"),
        "@REMOTE_TREE@": args.remote_tree.rstrip("/"),
        "@INGRESS@": f"{ip}:{args.ingress_port}",
        "@SGLANG_VERSION@": args.sglang_version,
        "@VLLM_VERSION@": args.vllm_version,
        "@DEVICE_DIGEST@": device["digest"],
        "@MODELS_ROOT@": args.models_root.rstrip("/"),
        "@SGLANG_VENV@": args.sglang_venv.rstrip("/"),
        "@VLLM_VENV@": args.vllm_venv.rstrip("/"),
        "@MANAGED_LIMIT@": budget["managed_limit"],
        "@FREE_RESERVE@": budget["free_reserve"],
        "@PARKED_LIMIT@": budget["parked_limit"],
        "@HOST_KV_LIMIT@": budget["host_kv_limit"],
        "@GPU_UUID@": uuid,
        "@MAX_PARKED@": int(budget["max_parked"]),
    }
    document = substitute(template, values)
    if args.managed_runtime:
        del document["runtime_dir"]
    if args.hf_max_bytes:
        document["model_sources"] = {"huggingface": "allowed", "max_bytes": args.hf_max_bytes}
    profiles = document["runtime_profiles"]
    if args.approve_speculation:
        if "vllm" in profiles:
            profiles["vllm"]["security"]["approved_options"] = ["--speculative-config"]
            # A draft model named in --speculative-config must lie here too.
            profiles["vllm"]["security"]["approved_paths"] = [args.models_root.rstrip("/")]
        if "sglang" in profiles:
            profiles["sglang"]["security"]["approved_options"] = ["--speculative-draft-model-path"]
            profiles["sglang"]["security"]["approved_paths"] = [args.models_root.rstrip("/")]
    if args.cuda_home:
        for profile in profiles.values():
            profile["cuda_home"] = args.cuda_home.rstrip("/")
    if args.no_profiles:
        document["runtime_profiles"] = {}
    fd = os.open(args.out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        json.dump(document, handle, indent=1)
        handle.write("\n")
    os.chmod(args.out, 0o600)
    print(json.dumps({"host": host, "policy": args.policy, "gpu": uuid, "digest": device["digest"],
                      "managed_limit": budget["managed_limit"], "out": args.out}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
