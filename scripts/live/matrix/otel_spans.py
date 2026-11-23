#!/usr/bin/env python3
"""Evidence summaries of the ADR 0037 live rows OT1-OT11, from the collector's file.

Runs on the control host over the files a row fetched from the host
(rows/OT*.sh, otel_rows.sh ot_summary). The collector's file exporter writes
OTLP JSON, one export request per line (trace and span IDs in hex). These
summaries state what the file holds; whether a row passed is judged from them
against ADR 0037's table.

  summary    spans per service and name; resource and attribute keys outside
             §5's allow-list (CapyCTL's services only); per request of the
             probe's records (otel_probe.py chat or load), found by
             `capyctl.request_id` equal to its X-CapyCTL-Request-Id: the
             request span's trace and parent against the caller's context, its
             links, every span of its trace with its parent's name (engine
             spans included, with `gen_ai.request.id` against the request ID),
             `first_progress` on each forward, the activation each
             `activation_wait` links to, and the span durations beside the
             answer's x-capyctl-timing phases; per activation trace its steps.
             A short text summary goes to standard output, the whole to --out.
  sentinels  how many times each sentinel occurs in the files' bytes (OT3).
  cost       OT8's arms side by side from otel_probe.py load summaries.

Standard library only.
"""

import argparse
import json
import os
import sys
from collections import Counter, defaultdict

# ADR 0037 §5: the only attributes CapyCTL spans carry.
ALLOWED_ATTRIBUTES = {
    "capyctl.request_id", "capyctl.endpoint", "capyctl.deployment", "capyctl.revision", "capyctl.instance",
    "capyctl.host", "capyctl.generation", "capyctl.attempt", "capyctl.engine", "capyctl.engine_version",
    "capyctl.stream", "capyctl.outcome", "capyctl.code", "capyctl.cancel_cause", "capyctl.activation.kind",
    "capyctl.activation.started", "capyctl.park_tier", "capyctl.victim", "http.request.method", "http.route",
    "http.response.status_code", "gen_ai.usage.input_tokens", "gen_ai.usage.output_tokens",
    "gen_ai.response.finish_reasons",
}
# §5: the resource of a CapyCTL role.
ALLOWED_RESOURCE = {"service.name", "service.version", "capyctl.role"}
CAPYCTL_SERVICES = {"capyctl-server", "capyctl-host", "capyctl-standalone"}
KINDS = {0: "unspecified", 1: "internal", 2: "server", 3: "client", 4: "producer", 5: "consumer"}
# x-capyctl-timing phase -> the span measuring the same interval (§3).
TIMING_SPANS = (("activation_wait_ms", "capyctl.activation_wait"), ("queue_wait_ms", "capyctl.queue_wait"),
                ("total_ms", "capyctl.request"))
# OT8: the router phases set side by side.
COST_PHASES = ("selection_ms", "lease_grant_ms", "pre_forward_ms", "time_to_first_byte_ms",
               "time_to_first_content_ms", "total_ms")


def value(v):
    """An OTLP JSON AnyValue as a Python value."""
    if not isinstance(v, dict):
        return v
    if "stringValue" in v:
        return v["stringValue"]
    if "boolValue" in v:
        return v["boolValue"]
    if "intValue" in v:
        return int(v["intValue"])
    if "doubleValue" in v:
        return float(v["doubleValue"])
    if "arrayValue" in v:
        return [value(x) for x in (v["arrayValue"].get("values") or [])]
    if "kvlistValue" in v:
        return attributes(v["kvlistValue"].get("values"))
    if "bytesValue" in v:
        return v["bytesValue"]
    return None


def attributes(items):
    return {a.get("key"): value(a.get("value")) for a in items or []}


def kind_name(kind):
    if isinstance(kind, str):
        return kind.replace("SPAN_KIND_", "").lower()
    return KINDS.get(kind, str(kind))


def load_spans(paths):
    """Every span in the files, flattened, and what could not be read."""
    spans, problems = [], []
    for path in paths:
        if not os.path.exists(path):
            problems.append({"file": path, "missing": True})
            continue
        with open(path, "rb") as handle:
            for number, raw in enumerate(handle, 1):
                if not raw.strip():
                    continue
                try:
                    doc = json.loads(raw)
                except ValueError:
                    problems.append({"file": path, "line": number, "unparsable": True})
                    continue
                for rs in doc.get("resourceSpans") or []:
                    resource = attributes((rs.get("resource") or {}).get("attributes"))
                    service = resource.get("service.name", "?")
                    for ss in rs.get("scopeSpans") or []:
                        scope = (ss.get("scope") or {}).get("name", "")
                        for sp in ss.get("spans") or []:
                            spans.append({
                                "trace": (sp.get("traceId") or "").lower(),
                                "span": (sp.get("spanId") or "").lower(),
                                "parent": (sp.get("parentSpanId") or "").lower(),
                                "name": sp.get("name", ""),
                                "kind": kind_name(sp.get("kind", 0)),
                                "start": int(sp.get("startTimeUnixNano") or 0),
                                "end": int(sp.get("endTimeUnixNano") or 0),
                                "attrs": attributes(sp.get("attributes")),
                                "events": [{"name": e.get("name"), "attrs": attributes(e.get("attributes"))}
                                           for e in sp.get("events") or []],
                                "links": [{"trace": (lk.get("traceId") or "").lower(),
                                           "span": (lk.get("spanId") or "").lower(),
                                           "attrs": attributes(lk.get("attributes"))}
                                          for lk in sp.get("links") or []],
                                "status": sp.get("status") or {},
                                "service": service, "resource": resource, "scope": scope,
                            })
    return spans, problems


def is_capyctl(span):
    return span["service"] in CAPYCTL_SERVICES or span["name"].startswith("capyctl.")


def ms(span):
    return round((span["end"] - span["start"]) / 1e6, 3) if span["end"] else None


class Index:
    def __init__(self, spans):
        self.spans = spans
        self.by_id = {(s["trace"], s["span"]): s for s in spans}
        self.by_trace = defaultdict(list)
        for s in spans:
            self.by_trace[s["trace"]].append(s)
        for trace in self.by_trace.values():
            # Start order, an enclosing span before the spans it contains.
            trace.sort(key=lambda s: (s["start"], -s["end"], s["name"]))

    def parent_of(self, span):
        return self.by_id.get((span["trace"], span["parent"])) if span["parent"] else None

    def parent_label(self, span, caller=None):
        """The parent's name, `caller` for the caller's own span, `root`, or `missing:<id>`."""
        if not span["parent"]:
            return "root"
        parent = self.parent_of(span)
        if parent:
            return parent["name"]
        if caller and caller.get("span_id") == span["parent"]:
            return "caller"
        return "missing:" + span["parent"]

    def tree(self, trace, caller=None):
        return [{"name": s["name"], "service": s["service"], "kind": s["kind"],
                 "parent": self.parent_label(s, caller), "ms": ms(s)} for s in self.by_trace.get(trace, [])]


def outside_allow_list(spans):
    """Keys of CapyCTL spans, events, links and resources that §5 does not allow."""
    attrs, resource = defaultdict(set), defaultdict(set)
    for s in spans:
        if not is_capyctl(s):
            continue
        for key in s["attrs"]:
            if key not in ALLOWED_ATTRIBUTES:
                attrs[s["name"]].add(key)
        for e in s["events"]:
            for key in e["attrs"]:
                attrs[f"{s['name']} event {e['name']}"].add(key)
        for lk in s["links"]:
            for key in lk["attrs"]:
                if key not in ALLOWED_ATTRIBUTES:
                    attrs[f"{s['name']} link"].add(key)
        for key in s["resource"]:
            if key not in ALLOWED_RESOURCE:
                resource[s["service"]].add(key)
    return ({k: sorted(v) for k, v in sorted(attrs.items())}, {k: sorted(v) for k, v in sorted(resource.items())})


def activation_of(index, link):
    """The activation trace a link points into: its root and its steps."""
    steps = index.by_trace.get(link["trace"], [])
    root = next((s for s in steps if s["name"] == "capyctl.activation"), None)
    return {"trace_id": link["trace"], "linked_span": (index.by_id.get((link["trace"], link["span"])) or {}).get("name"),
            "kind": (root or {}).get("attrs", {}).get("capyctl.activation.kind"),
            "steps": [s["name"] for s in steps]}


def request_entry(index, rec):
    rid = rec.get("request_id")
    caller = rec.get("caller") or {}
    entry = {"label": rec.get("label"), "request_id": rid, "status": rec.get("status"),
             "caller_mode": caller.get("mode")}
    found = [s for s in index.spans if s["name"] == "capyctl.request" and rid
             and s["attrs"].get("capyctl.request_id") == rid]
    entry["request_spans"] = len(found)
    if not found:
        return entry
    r = found[0]
    entry.update(
        trace_id=r["trace"], parent_span_id=r["parent"] or None,
        in_caller_trace=(r["trace"] == caller["trace_id"]) if caller.get("trace_id") else None,
        parent_is_caller_span=(r["parent"] == caller["span_id"]) if caller.get("span_id") else None,
        links=[{"trace_id": lk["trace"], "span_id": lk["span"]} for lk in r["links"]],
        link_to_caller=any(lk["trace"] == caller.get("trace_id") and lk["span"] == caller.get("span_id")
                           for lk in r["links"]) if caller.get("trace_id") else None,
        attributes=r["attrs"], tree=index.tree(r["trace"], caller))
    trace = index.by_trace[r["trace"]]
    entry["first_progress"] = [any(e["name"] == "first_progress" for e in s["events"])
                               for s in trace if s["name"] == "capyctl.forward"]
    entry["engine_spans"] = []
    for s in trace:
        if is_capyctl(s):
            continue
        parent = index.parent_of(s)
        gen_ai_id = s["attrs"].get("gen_ai.request.id")
        entry["engine_spans"].append({
            "service": s["service"], "name": s["name"], "parent": index.parent_label(s, caller),
            "parent_service": (parent or {}).get("service"),
            "gen_ai.request.id_equals_request_id": (gen_ai_id == rid) if gen_ai_id is not None else None})
    entry["activations"] = [dict(activation_of(index, lk), started=s["attrs"].get("capyctl.activation.started"))
                            for s in trace if s["name"] == "capyctl.activation_wait" for lk in s["links"]]
    timing = rec.get("timing") or {}
    compare = []
    for phase, name in TIMING_SPANS:
        span = next((s for s in trace if s["name"] == name), None)
        if timing.get(phase) is not None and span is not None:
            compare.append({"phase": phase, "span": name, "timing_ms": timing[phase], "span_ms": ms(span),
                            "diff_ms": round(ms(span) - timing[phase], 3)})
    entry["timing_vs_spans"] = compare
    return entry


def summarize(spans, requests, label_prefix=None):
    index = Index(spans)
    by_service = defaultdict(Counter)
    for s in spans:
        by_service[s["service"]][s["name"]] += 1
    attrs, resource = outside_allow_list(spans)
    records = [r for r in requests if not label_prefix or str(r.get("label", "")).startswith(label_prefix)]
    activations = []
    for s in spans:
        if s["name"] != "capyctl.activation":
            continue
        activations.append({"trace_id": s["trace"], "kind": s["attrs"].get("capyctl.activation.kind"),
                            "victim": s["attrs"].get("capyctl.victim"), "park_tier": s["attrs"].get("capyctl.park_tier"),
                            "deployment": s["attrs"].get("capyctl.deployment"), "ms": ms(s),
                            "steps": index.tree(s["trace"])})
    return {
        "spans": len(spans),
        "traces": len(index.by_trace),
        "by_service": {k: dict(sorted(v.items())) for k, v in sorted(by_service.items())},
        "resource_keys": {svc: sorted({k for s in spans if s["service"] == svc for k in s["resource"]})
                          for svc in sorted(by_service)},
        "attribute_keys_outside_allow_list": attrs,
        "resource_keys_outside_allow_list": resource,
        "requests": [request_entry(index, r) for r in records],
        "activations": activations,
    }


def brief(summary):
    """A few lines a reader can scan: services, then one line per request."""
    lines = [f"spans {summary['spans']} in {summary['traces']} traces"]
    for svc, names in summary["by_service"].items():
        lines.append(f"  {svc}: " + ", ".join(f"{n} {c}" for n, c in names.items()))
    if summary["attribute_keys_outside_allow_list"] or summary["resource_keys_outside_allow_list"]:
        lines.append("  keys outside the allow-list: " + json.dumps(
            [summary["attribute_keys_outside_allow_list"], summary["resource_keys_outside_allow_list"]]))
    for r in summary["requests"]:
        if not r["request_spans"]:
            lines.append(f"  {r['label']} {r['request_id']} status {r['status']}: no capyctl.request span")
            continue
        where = {True: "caller's trace", False: "new trace", None: "new trace"}[r["in_caller_trace"]]
        parent = {True: "caller's span", False: "not the caller's span", None: r["parent_span_id"] or "root"}[
            r["parent_is_caller_span"]]
        engines = ", ".join(f"{e['service']}:{e['name']}<-{e['parent']}" for e in r["engine_spans"]) or "none"
        acts = ", ".join(f"{a['kind']}({' '.join(a['steps'])})" for a in r["activations"]) or "none"
        lines.append(f"  {r['label']} {r['request_id']} status {r['status']} caller {r['caller_mode']}: {where}, "
                     f"parent {parent}, links {len(r['links'])}, spans "
                     f"{' '.join(t['name'] + '<-' + t['parent'] for t in r['tree'] if t['service'] in CAPYCTL_SERVICES)}; "
                     f"engine {engines}; activation {acts}")
        for c in r["timing_vs_spans"]:
            lines.append(f"    {c['phase']} {c['timing_ms']} ms, {c['span']} {c['span_ms']} ms, diff {c['diff_ms']} ms")
    for a in summary["activations"]:
        lines.append(f"  activation {a['kind']} {a['deployment']} victim {a['victim']} {a['ms']} ms: "
                     + " ".join(f"{s['name']}<-{s['parent']}" for s in a["steps"]))
    return "\n".join(lines)


def read_jsonl(paths):
    out = []
    for path in paths:
        if not os.path.exists(path):
            continue
        with open(path) as handle:
            out += [json.loads(line) for line in handle if line.strip()]
    return out


def sentinel_counts(paths, sentinels):
    data = b""
    for path in paths:
        if os.path.exists(path):
            with open(path, "rb") as handle:
                data += handle.read()
    low = data.lower()
    return {"bytes": len(data),
            "counts": {s: data.count(s.encode()) for s in sentinels},
            "counts_ignoring_case": {s: low.count(s.lower().encode()) for s in sentinels}}


def cost_table(arms):
    """OT8: each arm's throughput and router phases, and the change against the first arm."""
    rows, base = [], None
    for label, path in arms:
        with open(path) as handle:
            s = json.load(handle)
        row = {"arm": label, "n": s.get("n"), "ok": s.get("ok"), "concurrency": s.get("concurrency"),
               "traceparent": s.get("traceparent"), "output_tokens_per_s": s.get("output_tokens_per_s"),
               "latency_p50_ms": (s.get("latency_ms") or {}).get("p50"),
               "phases_p50_ms": {p: (s.get("timing") or {}).get(p, {}).get("p50") for p in COST_PHASES},
               "phases_p99_ms": {p: (s.get("timing") or {}).get(p, {}).get("p99") for p in COST_PHASES}}
        if base is None:
            base = row
        else:
            tps, btps = row["output_tokens_per_s"], base["output_tokens_per_s"]
            row["vs_" + base["arm"]] = {
                "output_tokens_per_s_pct": round(100.0 * (tps - btps) / btps, 2) if tps and btps else None,
                "phases_p50_diff_ms": {p: round(row["phases_p50_ms"][p] - base["phases_p50_ms"][p], 3)
                                       for p in COST_PHASES
                                       if row["phases_p50_ms"][p] is not None and base["phases_p50_ms"][p] is not None}}
        rows.append(row)
    return rows


def write(path, doc):
    if path:
        with open(path, "w") as handle:
            json.dump(doc, handle, indent=1, sort_keys=True)
            handle.write("\n")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("summary")
    s.add_argument("--collector", action="append", required=True, help="a collector file (repeatable)")
    s.add_argument("--requests", action="append", default=[], help="otel_probe.py records (repeatable)")
    s.add_argument("--label-prefix", help="only the requests whose label starts with this")
    s.add_argument("--out")
    s = sub.add_parser("sentinels")
    s.add_argument("--collector", action="append", required=True)
    s.add_argument("--sentinel", action="append", required=True)
    s.add_argument("--out")
    s = sub.add_parser("cost")
    s.add_argument("--arm", action="append", required=True, help="label=path of a load summary (repeatable)")
    s.add_argument("--out")
    args = p.parse_args()
    if args.cmd == "summary":
        spans, problems = load_spans(args.collector)
        doc = summarize(spans, read_jsonl(args.requests), args.label_prefix)
        doc["files"] = args.collector
        doc["problems"] = problems
        write(args.out, doc)
        print(brief(doc))
        for problem in problems:
            print("  file problem: " + json.dumps(problem))
        return 0
    if args.cmd == "sentinels":
        doc = sentinel_counts(args.collector, args.sentinel)
        doc["files"] = args.collector
        write(args.out, doc)
        print(json.dumps(doc, sort_keys=True))
        return 0
    arms = [tuple(a.split("=", 1)) for a in args.arm]
    doc = cost_table(arms)
    write(args.out, doc)
    for row in doc:
        print(json.dumps(row, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
