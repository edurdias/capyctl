"""Tests for otel_probe.py and otel_spans.py (ADR 0037 rows OT1-OT11). Standard library only.

    python3 -m unittest discover -s scripts/live/matrix/tests

A synthetic collector file (OTLP JSON, as the collector's file exporter writes
it) and fake records stand in for a live run. These tests check the helpers'
parsing and arithmetic only; they are not evidence for any row.
"""

import json
import os
import re
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import otel_probe  # noqa: E402
import otel_spans  # noqa: E402

CALLER_TRACE = "a" * 32
CALLER_SPAN = "b" * 16
RID = "01K7ZQ3M4X9V2B8N6C5D1F0G7H"


def s(v):
    return {"stringValue": v}


def span(trace, sid, parent, name, start, end, attrs=None, events=None, links=None, kind=1):
    return {"traceId": trace, "spanId": sid, "parentSpanId": parent, "name": name, "kind": kind,
            "startTimeUnixNano": str(start), "endTimeUnixNano": str(end),
            "attributes": [{"key": k, "value": v} for k, v in (attrs or {}).items()],
            "events": events or [], "links": links or []}


def export(service, spans, extra_resource=None):
    resource = [{"key": "service.name", "value": s(service)}, {"key": "capyctl.role", "value": s("standalone")}]
    resource += [{"key": k, "value": v} for k, v in (extra_resource or {}).items()]
    return {"resourceSpans": [{"resource": {"attributes": resource},
                               "scopeSpans": [{"scope": {"name": "capyctl"}, "spans": spans}]}]}


def collector_file(path):
    ms = 1_000_000
    act_trace = "c" * 32
    capyctl = [
        span(CALLER_TRACE, "1" * 16, CALLER_SPAN, "capyctl.request", 0, 120 * ms,
             {"capyctl.request_id": s(RID), "capyctl.endpoint": s("chat")}, kind=2),
        span(CALLER_TRACE, "2" * 16, "1" * 16, "capyctl.admission", 0, 1 * ms),
        span(CALLER_TRACE, "3" * 16, "1" * 16, "capyctl.activation_wait", 1 * ms, 81 * ms,
             {"capyctl.activation.started": {"boolValue": True}},
             links=[{"traceId": act_trace, "spanId": "9" * 16}]),
        span(CALLER_TRACE, "4" * 16, "1" * 16, "capyctl.forward", 82 * ms, 119 * ms,
             {"capyctl.attempt": {"intValue": "1"}, "prompt": s("leak")},
             events=[{"name": "first_progress", "timeUnixNano": str(90 * ms)}], kind=3),
        span(act_trace, "9" * 16, "", "capyctl.activation", 0, 80 * ms,
             {"capyctl.activation.kind": s("wake"), "capyctl.deployment": s("ot4-v")}),
        span(act_trace, "8" * 16, "9" * 16, "capyctl.wake", 0, 60 * ms),
        span(act_trace, "7" * 16, "9" * 16, "capyctl.canary", 60 * ms, 80 * ms),
    ]
    engine = [span(CALLER_TRACE, "5" * 16, "4" * 16, "llm_request", 83 * ms, 118 * ms,
                   {"gen_ai.request.id": s(RID), "host_id": s("deadbeef")}, kind=2)]
    with open(path, "w") as handle:
        handle.write(json.dumps(export("capyctl-standalone", capyctl, {"host.name": s("gpu")})) + "\n")
        handle.write(json.dumps(export("sglang", engine)) + "\n")
        handle.write("\n")


class SpansTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "collector.jsonl")
        collector_file(self.path)

    def tearDown(self):
        self.dir.cleanup()

    def test_load_flattens_every_span(self):
        spans, problems = otel_spans.load_spans([self.path, os.path.join(self.dir.name, "absent.jsonl")])
        self.assertEqual(len(spans), 8)
        self.assertEqual(problems, [{"file": os.path.join(self.dir.name, "absent.jsonl"), "missing": True}])
        forward = next(sp for sp in spans if sp["name"] == "capyctl.forward")
        self.assertEqual(forward["kind"], "client")
        self.assertEqual(forward["attrs"]["capyctl.attempt"], 1)

    def test_request_found_by_id_in_the_callers_trace(self):
        spans, _ = otel_spans.load_spans([self.path])
        rec = {"label": "ot4-wake", "request_id": RID, "status": 200,
               "caller": {"mode": "sampled", "trace_id": CALLER_TRACE, "span_id": CALLER_SPAN},
               "timing": {"activation_wait_ms": 80.2, "total_ms": 119.5, "queue_wait_ms": None}}
        summary = otel_spans.summarize(spans, [rec, {"label": "other", "request_id": "01X", "status": 200}])
        r = summary["requests"][0]
        self.assertEqual(r["request_spans"], 1)
        self.assertTrue(r["in_caller_trace"])
        self.assertTrue(r["parent_is_caller_span"])
        self.assertEqual(r["first_progress"], [True])
        self.assertEqual(r["engine_spans"], [{"service": "sglang", "name": "llm_request", "parent": "capyctl.forward",
                                              "parent_service": "capyctl-standalone",
                                              "gen_ai.request.id_equals_request_id": True}])
        self.assertEqual(r["activations"][0]["kind"], "wake")
        self.assertEqual(r["activations"][0]["steps"], ["capyctl.activation", "capyctl.wake", "capyctl.canary"])
        self.assertTrue(r["activations"][0]["started"])
        self.assertEqual({c["phase"]: c["diff_ms"] for c in r["timing_vs_spans"]},
                         {"activation_wait_ms": -0.2, "total_ms": 0.5})
        self.assertEqual(summary["requests"][1]["request_spans"], 0)
        self.assertEqual(summary["attribute_keys_outside_allow_list"], {"capyctl.forward": ["prompt"]})
        self.assertEqual(summary["resource_keys_outside_allow_list"], {"capyctl-standalone": ["host.name"]})
        self.assertEqual(summary["activations"][0]["deployment"], "ot4-v")
        self.assertIn("caller's trace, parent caller's span", otel_spans.brief(summary))

    def test_a_malformed_context_reads_as_a_new_trace(self):
        spans, _ = otel_spans.load_spans([self.path])
        rec = {"label": "bad", "request_id": RID, "caller": {"mode": "upper", "trace_id": "d" * 32,
                                                             "span_id": "e" * 16}}
        r = otel_spans.summarize(spans, [rec])["requests"][0]
        self.assertFalse(r["in_caller_trace"])
        self.assertFalse(r["parent_is_caller_span"])
        self.assertEqual(r["tree"][0]["parent"], "missing:" + CALLER_SPAN)

    def test_label_prefix_filters_requests(self):
        spans, _ = otel_spans.load_spans([self.path])
        recs = [{"label": "a1-x", "request_id": RID}, {"label": "a2-x", "request_id": RID}]
        self.assertEqual([r["label"] for r in otel_spans.summarize(spans, recs, "a2")["requests"]], ["a2-x"])

    def test_sentinels_are_counted_in_the_raw_bytes(self):
        doc = otel_spans.sentinel_counts([self.path], ["DEADBEEF", "OT3ABSENT"])
        self.assertEqual(doc["counts"], {"DEADBEEF": 0, "OT3ABSENT": 0})
        self.assertEqual(doc["counts_ignoring_case"], {"DEADBEEF": 1, "OT3ABSENT": 0})

    def test_cost_compares_each_arm_with_the_first(self):
        paths = []
        for label, tps, pre in (("off", 100.0, 1.0), ("sampled", 95.0, 1.25)):
            path = os.path.join(self.dir.name, f"{label}.json")
            with open(path, "w") as handle:
                json.dump({"n": 200, "ok": 200, "output_tokens_per_s": tps,
                           "timing": {"pre_forward_ms": {"p50": pre, "p99": pre * 2}}}, handle)
            paths.append((label, path))
        rows = otel_spans.cost_table(paths)
        self.assertEqual(rows[1]["vs_off"], {"output_tokens_per_s_pct": -5.0,
                                             "phases_p50_diff_ms": {"pre_forward_ms": 0.25}})


class ProbeTest(unittest.TestCase):
    def test_trace_contexts(self):
        valid = re.compile(r"^00-[0-9a-f]{32}-[0-9a-f]{16}-0[01]$")
        headers, rec = otel_probe.trace_context("sampled")
        self.assertRegex(headers[0], valid)
        self.assertTrue(headers[0].endswith("-01") and rec["valid"] and rec["sampled"])
        self.assertEqual(headers[0], f"00-{rec['trace_id']}-{rec['span_id']}-01")
        headers, rec = otel_probe.trace_context("unsampled")
        self.assertTrue(headers[0].endswith("-00") and not rec["sampled"])
        self.assertEqual(otel_probe.trace_context("none"), ([], {"mode": "none", "valid": False}))
        for mode in otel_probe.MALFORMED:
            headers, rec = otel_probe.trace_context(mode)
            self.assertFalse(rec["valid"], mode)
            if mode == "twice":
                self.assertEqual(len(headers), 2)
                self.assertTrue(all(valid.match(h) for h in headers))
            else:
                self.assertEqual(len(headers), 1)
                self.assertFalse(valid.match(headers[0]) and "-" + "0" * 16 + "-" not in headers[0]
                                 and "-" + "0" * 32 + "-" not in headers[0], mode)

    def test_sse_keeps_usage_and_the_timing_comment(self):
        lines = ['data: {"choices":[{"delta":{"role":"assistant"}}]}\n', "\n",
                 'data: {"choices":[{"delta":{"content":"4"}}]}\n', "\n",
                 'data: {"choices":[{"delta":{"content":"2"},"finish_reason":"stop"}]}\n', "\n",
                 'data: {"choices":[],"usage":{"completion_tokens":2}}\n', "\n",
                 ': x-capyctl-timing {"total_ms": 12.5}\n', "\n", "data: [DONE]\n"]
        out = otel_probe.parse_sse(lines)
        self.assertEqual((out["content"], out["finish_reason"], out["done"]), ("42", "stop", True))
        self.assertEqual(out["usage"], {"completion_tokens": 2})
        self.assertEqual(out["timing"], {"total_ms": 12.5})
        self.assertEqual(out["first_content_index"], 2)

    def test_chat_body_places_each_sentinel(self):
        class Args:
            route, model, prompt, max_tokens, stream = "ot3-v", None, "hi", 16, False
            sentinel_message, sentinel_system, sentinel_tool = "SMSG", "SSYS", "STOOL"
            cache_salt, engine_error, engine_error_kind = "SSALT", "SERR", "tool_choice"
        text = json.dumps(otel_probe.chat_body(Args))
        for marker in ("SMSG", "SSYS", "STOOL", "SSALT", "SERR"):
            self.assertIn(marker, text)
        self.assertEqual(json.loads(text)["tool_choice"]["function"]["name"], "SERR")

    def test_tracing_options_and_names(self):
        argv = ["python3", "-m", "sglang.launch_server", "--enable-trace", "--otlp-traces-endpoint",
                "http://127.0.0.1:4318/v1/traces", "--otlp-service-name=sglang", "--port", "8100"]
        self.assertEqual(otel_probe.tracing_args(argv), [["--enable-trace", ""],
                                                         ["--otlp-traces-endpoint", "http://127.0.0.1:4318/v1/traces"],
                                                         ["--otlp-service-name", "sglang"]])
        env = otel_probe.tracing_env({"SGLANG_TRACE_LEVEL": "1", "OTEL_EXPORTER_OTLP_HEADERS": "x=secret",
                                      "PATH": "/bin", "traceparent": "00-x"})
        self.assertEqual(env, {"SGLANG_TRACE_LEVEL": "1", "OTEL_EXPORTER_OTLP_HEADERS": "<set>", "traceparent": "<set>"})
        self.assertEqual(otel_probe.classify("/home/u/capyctl start standalone --set x=y"), "capyctl")
        self.assertEqual(otel_probe.classify("/v/bin/python3 -m sglang.launch_server --port 1"), "engine")
        self.assertIsNone(otel_probe.classify("python3 otel_probe.py procs"))
        self.assertIsNone(otel_probe.classify("bash /t/role_exec.sh p l /home/u/capyctl start standalone"))
        self.assertIsNone(otel_probe.classify("tmux -L capyctl-mx new-session -d -s x /home/u/capyctl start host"))

    def test_ss_lines_on_the_collector_ports(self):
        text = ("LISTEN 0 4096 127.0.0.1:4318 0.0.0.0:* users:((\"otelcol\",pid=10,fd=7))\n"
                "ESTAB 0 0 127.0.0.1:50000 127.0.0.1:4318 users:((\"capyctl\",pid=20,fd=9))\n"
                "ESTAB 0 0 127.0.0.1:8443 127.0.0.1:50001 users:((\"capyctl\",pid=20,fd=10))\n")
        socks = otel_probe.parse_ss(text, {"4317", "4318"})
        self.assertEqual(len(socks), 2)
        self.assertEqual([x["to_collector"] for x in socks], [False, True])
        self.assertEqual(socks[1]["process"], [{"name": "capyctl", "pid": 20}])

    def test_packages_by_files_only(self):
        with tempfile.TemporaryDirectory() as venv:
            site = os.path.join(venv, "lib", "python3.12", "site-packages")
            os.makedirs(site)
            self.assertFalse(otel_probe.tracing_packages(venv)["complete"])
            for name in otel_probe.DISTRIBUTIONS:
                os.makedirs(os.path.join(site, f"{name}-1.0.dist-info"))
            for module in otel_probe.MODULES:
                os.makedirs(os.path.dirname(os.path.join(site, module)), exist_ok=True)
                open(os.path.join(site, module), "w").close()
            self.assertEqual(otel_probe.tracing_packages(venv)["missing"], [])
            os.remove(os.path.join(site, otel_probe.MODULES[-1]))
            self.assertEqual(otel_probe.tracing_packages(venv)["missing"], [otel_probe.MODULES[-1]])

    def test_load_summary(self):
        recs = [{"status": 200, "done": True, "ms": 100.0 + i, "usage": {"completion_tokens": 10},
                 "timing": {"pre_forward_ms": 1.0 + i / 10}} for i in range(10)]
        recs.append({"status": 503, "ms": 5.0})
        out = otel_probe.load_summary("x", recs, 2.0, "sampled", 4)
        self.assertEqual((out["n"], out["ok"], out["output_tokens"], out["output_tokens_per_s"]), (11, 10, 100, 50.0))
        self.assertEqual(out["status_counts"], {"200": 10, "503": 1})
        self.assertEqual(out["timing"]["pre_forward_ms"]["n"], 10)
        self.assertEqual(otel_probe.gaps([0.0, 61.0, 125.5]), [61.0, 64.5])


if __name__ == "__main__":
    unittest.main()
