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
                       [--hf-max-bytes SIZE] [--approve-speculation] [--cuda-home DIR]
                       [--peer-address IP --rendezvous-ports START-END]
                       [--tensorfold-venv DIR --tensorfold-version V [--tensorfold-cuda-home DIR]]
                       --out FILE

--hf-max-bytes and --approve-speculation (2026-09-25, single-box benchmark)
opt the host into Hugging Face model sources with that store ceiling (ADR 0008
amendment) and approve the speculative-decoding options (vLLM
`--speculative-config` and SGLang `--speculative-draft-model-path`, with draft
models inside the model store; ADR 0014 section 8). --cuda-home sets every
profile's `cuda_home` (SPEC section 13.3 as amended 2026-09-25: `<dir>/bin` joins
the engine PATH; vLLM needs `nvcc` there for FlashInfer).

--peer-address and --rendezvous-ports (ADR 0028 section 3, rows MN1-MN8) write
`resource_policy.groups`: the address this host's group peers reach it on (its
direct-link address) and the inclusive range a group head takes its rendezvous
port from. --tensorfold-venv adds a `tensorfold` profile the way `capyctl engine
add` writes one (ADR 0018, ADR 0023): `<venv>/bin/tensorfold`, the installed
version as its build fingerprint, deep parking disabled, and cuda_home from
--tensorfold-cuda-home when given (TensorFold compiles its kernels with nvcc).
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
    parser.add_argument("--peer-address")
    parser.add_argument("--rendezvous-ports")
    parser.add_argument("--tensorfold-venv")
    parser.add_argument("--tensorfold-version")
    parser.add_argument("--tensorfold-cuda-home")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    with open(args.device_json) as handle:
        device = json.load(handle)
    if device.get("schema") != "capyctl-nvidia-inventory-v1":
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
    groups = None
    if bool(args.peer_address) != bool(args.rendezvous_ports):
        sys.exit("--peer-address and --rendezvous-ports go together")
    if args.peer_address:
        # ADR 0028 section 3: an IP its peers can reach, never loopback,
        # unspecified, multicast or broadcast (groups_policy.rs refuses those).
        peer = ipaddress.ip_address(args.peer_address)
        if peer.is_loopback or peer.is_unspecified or peer.is_multicast or str(peer) == "255.255.255.255":
            sys.exit(f"peer address {peer} cannot be a group peer address")
        match = re.fullmatch(r"(\d+)-(\d+)", args.rendezvous_ports)
        if not match or not 1024 <= int(match[1]) <= int(match[2]) <= 65535:
            sys.exit(f"rendezvous ports {args.rendezvous_ports!r} must be START-END with 1024 <= START <= END <= 65535")
        start, end = int(match[1]), int(match[2])
        if start <= args.ingress_port <= end or not (end < 8100 or start > 8199):
            sys.exit("the rendezvous range must miss the ingress port and the engine port range 8100-8199")
        groups = {"peer_address": str(peer), "rendezvous_port_range": {"start": start, "end": end}}
    if bool(args.tensorfold_venv) != bool(args.tensorfold_version):
        sys.exit("--tensorfold-venv and --tensorfold-version go together")
    if args.tensorfold_venv:
        if not VERSION.match(args.tensorfold_version):
            sys.exit(f"TensorFold version {args.tensorfold_version!r} is malformed")
        for path in filter(None, (args.tensorfold_venv, args.tensorfold_cuda_home)):
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
    if groups:
        document["resource_policy"]["groups"] = groups
    if args.tensorfold_venv:
        document["environment_fingerprint"] += f"-tensorfold-{args.tensorfold_version}"
        # registration.rs profile_document: what `engine add` writes for TensorFold.
        tensorfold = {
            "engine": "tensorfold",
            "revision": 1,
            "executable": args.tensorfold_venv.rstrip("/") + "/bin/tensorfold",
            "build_fingerprint": args.tensorfold_version,
            "args": [],
            "env": {},
            "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
            "security": {
                "deep_park": "disabled",
                "trust_remote_code": False,
                "credential_ref": "secret://engine-key",
                "admin_credential_ref": "secret://admin-key",
            },
        }
        if args.tensorfold_cuda_home:
            tensorfold["cuda_home"] = args.tensorfold_cuda_home.rstrip("/")
        document["runtime_profiles"]["tensorfold"] = tensorfold
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
