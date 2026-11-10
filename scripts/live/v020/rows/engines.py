"""P3 native-engine lifecycle and Clef decisions (not version certification)."""
import json
import re


ENGINES = ("vllm033", "sglang0522", "sglang0523", "llamacpp090", "tensorfold126", "TENSORRT_LLM_121", "TENSORRT_LLM_140RC3")
ROWS = [dict(id="P3-" + engine, priority=3, budget=600,
             criteria="Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang.",
             engine=engine) for engine in ENGINES]
ROWS += [dict(id="P3-decisions-" + engine, priority=3, budget=600,
              criteria="Clef-Flash becomes ready and answers /v1/systemone, then stops with verified exit.",
              engine=engine, decisions=True) for engine in ("sglang0523", "llamacpp090")]


def document(response):
    body = response.get("body")
    return json.loads(body) if isinstance(body, str) else body


def answered(ctx, response, stream=False):
    ctx.assertion(response.get("status") == 200, "request did not return HTTP 200")
    if stream:
        ctx.assertion(response.get("done") is True, "SSE stream missing terminal [DONE]")
        ctx.assertion(bool(response.get("chunks")), "SSE stream had no chunks")
    else:
        body = document(response)
        ctx.assertion(isinstance(body, dict) and bool(body.get("choices")), "chat response missing choices")
        ctx.assertion(bool(body["choices"][0].get("message")), "chat response missing message")


def stopped(ctx, name):
    ctx.stop(name)
    ctx.assertion(not ctx.owned(name), "stop did not establish owned processes gone")


def passing_canary(events):
    for event in events:
        text = json.dumps(event).lower().replace("_", " ")
        if ("canary" in text and re.search(r'\b(pass|passed|success|succeeded|matched)\b', text)
                and not re.search(r'\b(fail|failed|mismatch)\b', text)):
            return True
    return False


def run(ctx, row):
    engine = row["engine"]
    name = row["id"].lower().replace("_", "-")
    decisions = row.get("decisions", False)
    residency = "deep" if engine.startswith(("vllm", "sglang")) else "restart_only"
    evidence = {"engine": engine, "requests": []}
    if engine == "TENSORRT_LLM_140RC3":
        ctx.installation(engine)
        from .measurements import trt_preflight
        failure = trt_preflight(ctx)
        if failure:
            return failure
    try:
        profile = ctx.register(engine)
        model = "clef" if decisions else ("tensorfold" if engine == "tensorfold126" else "chat")
        ctx.deploy(name, engine, model=model, api="decisions" if decisions else "chat", residency=residency)
        listing = ctx.cli("engine", "list", "--json")
        evidence["registration"] = json.loads(listing.stdout)
        ctx.assertion(profile in listing.stdout, "registered profile absent from engine list")
        expected = {"vllm033": "0.33.0", "sglang0522": "0.5.22", "sglang0523": "0.5.23",
                    "llamacpp090": "0.9.0", "tensorfold126": "1.2.6"}.get(engine)
        if expected:
            ctx.assertion(re.search(r"(?<![0-9])" + re.escape(expected) + r"(?![0-9])", listing.stdout),
                          "registered engine list missing required version " + expected)
        evidence["ready"] = ctx.status(name)
        evidence["rendered"] = ctx.engine_args(name)
        ctx.assertion(bool(ctx.owned(name)), "ready deployment has no owned process evidence")
        ctx.assertion(evidence["ready"].get("observed_state", "").lower() == "ready", "deployment did not establish ready")
        if decisions:
            response = ctx.request(name, path="/v1/systemone", body={"model": name,
                "state": "The customer was billed twice for the same order.",
                "questions": {"billing": {"type": "noul", "instructions": "Is this about billing?"}}})
            evidence["requests"].append(response)
            ctx.assertion(response.get("status") == 200, "decision request failed")
            body = document(response)
            ctx.assertion(isinstance(body, dict) and "billing" in body.get("answers", {}), "decision missing billing answer")
        else:
            for stream in (False, True):
                response = ctx.request(name, stream=stream, body={"model": name,
                    "messages": [{"role": "user", "content": "Reply with one short greeting."}], "max_tokens": 32, "stream": stream})
                answered(ctx, response, stream)
                evidence["requests"].append(response)
            if residency == "deep":
                before = ctx.owned(name)
                event_offset = len(ctx.events())
                ctx.lifecycle("park", name)
                evidence["parked"] = ctx.status(name)
                ctx.assertion(evidence["parked"].get("observed_state", "").lower() == "parked", "parked state absent")
                ctx.assertion(bool(ctx.owned(name)), "deep park exited engine instead of retaining launch")
                ctx.lifecycle("wake", name)
                after = ctx.owned(name)
                ctx.assertion({p["pid"] for p in before} == {p["pid"] for p in after}, "deep wake replaced engine process")
                response = ctx.request(name)
                answered(ctx, response)
                evidence["requests"].append(response)
                events = ctx.events()[event_offset:]
                evidence["events"] = events
                ctx.assertion(passing_canary(events), "wake has no passing canary evidence")
        stopped(ctx, name)
        return dict(verdict="pass", reason="Lifecycle and endpoint assertions passed", evidence=evidence)
    except (AssertionError, ValueError, KeyError, TypeError) as error:
        return dict(verdict="fail", reason=str(error), evidence=evidence)
