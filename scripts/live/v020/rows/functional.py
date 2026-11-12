"""P4 contracts that can be established without fault-injection/collector helpers."""
import base64
import json
import math
import re
import struct

from .engines import document, passing_canary, stopped


SKIPS = {
    "EM3": "Needs pinned 100-sentence cross-engine fixture and concurrent 64-input canary protocol; ordinary request success cannot establish tolerance.",
    "EM5": "Needs client disconnect/deadline injection and lease/engine-connection settlement observation, unavailable through collected Context.request.",
    "EM6": "Needs separately enrolled host ingress and engine-side request-header observation.",
    "EM7": "Needs enforced mutually exclusive memory grants and pool-switch evidence, not two independently fitting deployments.",
    "EM9": "Needs per-launch parked GPU budget and SGLang saver-map unmapped evidence; status alone cannot prove deep release.",
    "OT11": "Needs isolated role/profile configuration refusal fixtures for non-loopback exporter and reserved tracing variables.",
    "TF1": "Exact ADR row needs 1.0.0 and 0.6.6 side-by-side registration and collision refusal; 1.2.6 lifecycle is separately P3, not equivalent.",
    "TF4": "Needs admission pressure fixture, scratch default-reserve launch and all native refusal cases, not only capability refusals.",
    "TF5": "Needs eight live streams, zero-gauge drain proof and a deliberately busy 30-second stop bound.",
    "TF6": "Needs client-cut protocol with 100 ms gauges, six repeats and 32-cancellation restart proof.",
    "TF7": "Needs enrolled host, per-pool metrics/histograms and cancelled-request remote settlement.",
    "TF8": "Needs constrained mutually exclusive TensorFold/vLLM grants and measured fresh-launch wake canary.",
    "TF9": "Requires approved TensorFold 0.6.6 environment, absent from fresh VM recipe.",
    "TF10": "Needs listener enumeration after readiness and a controller re-proof trigger; ownership PID alone is insufficient.",
    "TF11": "Exact ADR row requires 1.1.5 beside 1.0.0 and 0.6.6 with archive/kernel checksum verification.",
    "TF12": "Needs direct health progression and startup/device memory metrics for 1.1.5, plus reserved idle-unload refusal.",
    "TF13": "Needs in-stop health 503/draining observation, signal-to-exit timing and busy bound fixture.",
    "TF14": "Needs TF6 cancellation protocol and TF7 enrolled-host metrics/settlement on 1.1.5.",
    "TR1": "Needs wheel provenance, non-executing detection audit, CUDA/OpenMPI companion evidence and no-CUDA-context registration observation.",
    "TR2": "Needs direct version/readiness step evidence, rendered YAML and both tool-call/reasoning parser assertions; P3 chat is not equivalent.",
    "TR4": "Needs the single load-reader's burst/lull observations, not direct metrics polling that starts another reader.",
    "TR5": "Needs client-cut injection, engine abort timing, uncertain leases and 32-cut single-restart proof.",
    "TR6": "Needs per-process memory disappearance, five fresh-launch cycles and exact deep refusal code, beyond P3 verified stop.",
    "TR7": "Needs complete pre-launch refusal matrix including reserved LLM API nested keys and exact refusal codes.",
    "TR8": "Needs cold-start outbound sockets sampled each second and sanitized telemetry/offline environment-name assertions.",
    "TR9": "Needs owned-process SIGKILL, process-group/listener proof and release-order evidence.",
    "TR10": "Needs authorized unmanaged MPI baseline with process/session/listener inventory; managed throughput alone is insufficient.",
    "TR11": "Needs enrolled host ingress and TR5's cancellation settlement protocol.",
    "LC12": "Needs historical lmcache_driven rendering and GB10 fabric-handle readout; fresh VM is discrete and must not alter existing GB10 environments.",
    "LC17": "Needs isolated host_kv_limit configurations and LMCache token-hit/pool-charge evidence across capped and explicit pools.",
    "LC18": "Needs build rendering engine-driven restart transfer and 15-cycle LMCache hit, held-KV and stop-bound observations.",
}
for implemented in ("EM9", "TF1", "TF11", "OT11"):
    SKIPS.pop(implemented)
SKIPS = {key: "not implemented: " + reason for key, reason in SKIPS.items()}
ROWS = [dict(id=key, priority=4, budget=1, criteria=reason) for key, reason in SKIPS.items()]
ROWS += [dict(id=key, priority=4, budget=1200, criteria=criteria) for key, criteria in (
    ("EM2", "Full embedding input/encoding/dimension/error contract on vLLM, SGLang and llama.cpp."),
    ("EM4", "Restart each embeddings deployment and require a journaled passing reference canary."))]
ROWS += [dict(id=key, priority=4, budget=budget, criteria=criteria) for key,budget,criteria in (
    ("EM9",900,"Five deep embedding park/wake cycles and a parked-arrival wake on each Python engine; functional evidence without memory measurements."),
    ("TF1",120,"TensorFold 1.2.6 registration, archive checksums and native capabilities."),
    ("TF11",120,"TensorFold 1.1.5 registration beside 1.2.6, archive checksums and native capabilities."),
    ("OT11",300,"Non-loopback engine tracing and reserved profile argument/environment refusals."))]
EMBEDDING_ENGINES = ("vllm033", "sglang0523", "llamacpp090")


def vectors(ctx, response, name, count):
    ctx.assertion(response.get("status") == 200, "embedding request failed")
    body = document(response)
    ctx.assertion(body.get("model") == name, "embedding public model name not preserved")
    data = body.get("data", [])
    ctx.assertion(len(data) == count, "embedding count mismatch")
    ctx.assertion([item.get("index") for item in data] == list(range(count)), "embedding order/index mismatch")
    return [item["embedding"] for item in data]


def embedding(ctx, name, inputs, **fields):
    body = {"model": name, "input": inputs, **fields}
    response = ctx.request(name, path="/v1/embeddings", body=body)
    ctx.index += 1
    ctx.save("embedding-response-" + str(ctx.index), dict(path="/v1/embeddings", request=body, response=response))
    return response


def contract(ctx, name, engine):
    records = []
    for inputs, count in (("qualification probe", 1), (["first", "second"], 2), ([100, 101], 1), ([[100, 101], [102, 103]], 2)):
        result = embedding(ctx, name, inputs)
        values = vectors(ctx, result, name, count)
        ctx.assertion(all(isinstance(v, list) and v and all(isinstance(x, (float, int)) and math.isfinite(x) for x in v) for v in values), "invalid float vector")
        records.append({"count": count, "dimensions": [len(v) for v in values]})
    plain_response = embedding(ctx, name, "qualification probe", encoding_format="float")
    reference = vectors(ctx, plain_response, name, 1)[0]
    encoded = vectors(ctx, embedding(ctx, name, "qualification probe", encoding_format="base64"), name, 1)[0]
    raw = base64.b64decode(encoded, validate=True)
    ctx.assertion(len(raw) == 4 * len(reference), "base64 byte length mismatch")
    decoded = struct.unpack("<" + "f" * len(reference), raw)
    ctx.assertion(all(math.isclose(a, b, rel_tol=1e-6, abs_tol=1e-7) for a, b in zip(reference, decoded)), "base64 differs beyond float32 rounding")
    reduced = embedding(ctx, name, "qualification probe", dimensions=256)
    if engine.startswith("llama"):
        ctx.assertion(reduced.get("status") == 400 and "invalid_request" in json.dumps(reduced.get("body")),
                      "llama.cpp did not refuse non-native dimensions with invalid_request")
    else:
        ctx.assertion(len(vectors(ctx, reduced, name, 1)[0]) == 256, "dimension reduction not applied")
    for inputs, fields in (("probe", {"stream": True}), ("probe", {"unknown_field": 1}), ("", {})):
        result = embedding(ctx, name, inputs, **fields)
        ctx.assertion(result.get("status") == 400 and "invalid_request" in json.dumps(result.get("body")), "invalid input not refused by CapyCTL")
    result = embedding(ctx, name, [100] * 32769)
    ctx.assertion(result.get("status", 200) >= 400 and "engine_rejected" in json.dumps(result.get("body")), "overlength input missing engine_rejected")
    body = plain_response.get("body")
    keys = list(json.loads(body) if isinstance(body, str) else body)
    ctx.assertion("model" in keys and "data" in keys, "response field ordering unavailable")
    records.append({"model_before_data": keys.index("model") < keys.index("data"), "base64_max_abs_error": max(abs(a-b) for a,b in zip(reference, decoded))})
    return records

def restart_canary_verdict(events):
    # ADR 0036 EM4 explicitly requires a verdict event after stop/start.
    for event in events:
        text = json.dumps(event).lower().replace("_", " ")
        if ("canary" in text and re.search(r"\b(pass|passed|success|succeeded|matched)\b", text)
                and not re.search(r"\b(fail|failed|mismatch)\b", text)):
            return True
    return False



def run(ctx, row):
    if row["id"] in SKIPS:
        return dict(verdict="skip", reason=SKIPS[row["id"]], evidence={})
    evidence = {}
    try:
        if row["id"] == "EM9":
            return embedding_cycles(ctx, evidence)
        if row["id"] in ("TF1", "TF11"):
            return native_registration(ctx, row, evidence)
        if row["id"] == "OT11":
            return tracing_refusals(ctx, evidence)
        for engine in EMBEDDING_ENGINES:
            name = row["id"].lower() + "-" + engine
            extra = {}
            if row["id"] == "EM2" and engine.startswith(("vllm", "sglang")):
                # ADR 0036 §2: ordinary HF overrides enroll this Matryoshka model.
                flag = "--hf-overrides" if engine.startswith("vllm") else "--json-model-override-args"
                extra = {"engine_config": {"accept_extra_args": True,
                         "extra_args": [flag, '{"is_matryoshka":true}']}}
            ctx.deploy(name, engine, model="embedding", api="embeddings",
                       residency="restart_only", extra=extra)
            if extra:
                ctx.save(name + "-launch", dict(override=extra, settings=ctx.engine_args(name)))
            if row["id"] == "EM2":
                evidence[engine] = contract(ctx, name, engine)
            else:
                vectors(ctx, embedding(ctx, name, "qualification probe"), name, 1)
                before_events = len(ctx.events())
                stopped(ctx, name)
                ctx.lifecycle("start", name)
                vectors(ctx, embedding(ctx, name, "qualification probe"), name, 1)
                events = ctx.events()[before_events:]
                evidence[engine] = {"events": events, "canary": passing_canary(ctx, name, restore=False)}
                ctx.save(name + "-restart-evidence", evidence[engine])
                ctx.assertion(restart_canary_verdict(events), "restart missing passing reference-canary verdict event (ADR0036 EM4)")
            ctx.save(name + "-contract", evidence[engine])
            stopped(ctx, name)
        return dict(verdict="pass", reason="All three engine contracts passed", evidence=evidence)
    except (AssertionError, ValueError, TypeError, KeyError, struct.error) as error:
        return dict(verdict="fail", reason=str(error), evidence=evidence)


def embedding_cycles(ctx,evidence):
    engines = [
        "vllm033" if ctx.engines.get("VLLM_033") else "vllm032",
        "sglang0523" if ctx.engines.get("SGLANG_0523") else "sglang0522",
    ]
    for engine in engines:
        name="em9-"+engine
        ctx.deploy(name,engine,model="embedding",api="embeddings",residency="deep")
        baseline=vectors(ctx,embedding(ctx,name,"qualification probe"),name,1)[0]
        cycles=[]
        evidence[engine]=cycles
        for cycle in range(6):
            pids={item["pid"] for item in ctx.owned(name)}
            offset=len(ctx.events())
            ctx.lifecycle("park",name)
            parked=ctx.status(name)
            ctx.assertion(parked.get("observed_state","").lower()=="parked","deep park did not settle")
            if cycle<5:
                ctx.lifecycle("wake",name)
            response=embedding(ctx,name,"qualification probe")
            vector=vectors(ctx,response,name,1)[0]
            ctx.assertion(len(vector)==len(baseline),"wake changed native vector dimension")
            distance=math.sqrt(sum((a-b)**2 for a,b in zip(vector,baseline)))/max(math.sqrt(sum(a*a for a in baseline)),1e-30)
            ctx.assertion(distance<=.02,"wake embedding exceeds provisional canary tolerance")
            events=ctx.events()[offset:]
            canary=passing_canary(ctx,name)
            ctx.assertion(pids=={item["pid"] for item in ctx.owned(name)},"deep wake replaced process")
            ctx.assertion(ctx.status(name).get("observed_state","").lower()=="ready","parked-arrival request did not wake deployment")
            cycles.append(dict(cycle=cycle+1,parked=parked,events=events,canary=canary,relative_distance=distance,implicit_wake=cycle==5))
        stopped(ctx,name)
    return dict(verdict="skip",reason=("measurement not allowed on this host" if ctx.functional_only else "not implemented: parked GPU-budget/saver-map release measurement"),evidence=evidence)


def native_registration(ctx,row,evidence):
    import hashlib
    from pathlib import Path
    for key,version in (("TENSORFOLD_126","1.2.6"),("TENSORFOLD_115","1.1.5")):
        if row["id"]=="TF1" and key=="TENSORFOLD_115":
            continue
        value=ctx.installation(key)
        binary=Path(value)
        if binary.is_dir():
            binary=binary/"bin/tensorfold-native"
        root=binary.parent.parent
        ctx.assertion((root/"VERSION").read_text().strip().lstrip("v")==version,"native VERSION mismatch")
        manifest=root/"SHA256SUMS"
        ctx.assertion(manifest.is_file(),"native release lacks SHA256SUMS")
        checked=[]
        for line in manifest.read_text().splitlines():
            if not line.strip():
                continue
            digest,relative=line.split(maxsplit=1)
            path=(root/relative.lstrip(" *")).resolve()
            ctx.assertion(path.is_relative_to(root.resolve()) and path.is_file(),"checksum entry outside release or missing")
            actual=hashlib.sha256()
            with path.open("rb") as source:
                for block in iter(lambda:source.read(1024*1024),b""):
                    actual.update(block)
            ctx.assertion(actual.hexdigest()==digest,"native release checksum mismatch: "+relative)
            checked.append(relative)
        ctx.assertion(checked,"native checksum file empty")
        capabilities=json.loads(ctx.command([binary,"capabilities","--json"]).stdout)
        ctx.assertion(capabilities and version in json.dumps(capabilities),"native capabilities/version mismatch")
        detected=ctx.cli("engine","detect","--path",str(root),"--json")
        ctx.assertion(version in detected.stdout,"engine detect omitted native version")
        profile=ctx.register(key)
        listing=json.loads(ctx.cli("engine","list","--json").stdout)
        text=json.dumps(listing)
        ctx.assertion(profile in text and "native" in text and version in text,"native profile absent from registration list")
        evidence[key]=dict(version=version,checksummed_files=checked,capabilities=capabilities,registration=listing)
    return dict(verdict="pass",reason="Pinned native versions detected, checksummed and registered side by side",evidence=evidence)


def tracing_refusals(ctx,evidence):
    from pathlib import Path
    state=ctx.root/"ot11-refused-standalone"
    result=ctx.command([ctx.binary(),"--state-dir",state,"start","standalone",
        "--set","server.observability.tracing.endpoint=http://192.0.2.1:14318/v1/traces",
        "--set","host.observability.tracing.engines=on"],check=False)
    ctx.assertion(result.returncode!=0 and "loopback" in (result.stdout+result.stderr).lower(),"non-loopback engine tracing start not refused")
    evidence["non_loopback"]=dict(exit_code=result.returncode,reason="loopback")
    for key,flags,reason in (
        ("VLLM_033",["--arg=--otlp-traces-endpoint","--arg=http://127.0.0.1:14318/v1/traces"],"reserved"),
        ("SGLANG_0523",["--arg=--enable-trace"],"not accepted here"),
        ("VLLM_033",["--env=OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:14318"],"engine_env_reserved")):
        path=ctx.installation(key)
        result=ctx.cli("engine","add",path,"--name","ot11-refused",*flags,check=False)
        ctx.assertion(result.returncode!=0 and reason in (result.stdout+result.stderr),"reserved tracing profile setting accepted or wrong refusal")
        evidence.setdefault("profile_refusals",[]).append(dict(engine=key,exit_code=result.returncode,
                                                             reason=reason,stdout=result.stdout,stderr=result.stderr))
    return dict(verdict="pass",reason="Non-loopback exporter and reserved engine tracing profile settings refused",evidence=evidence)
