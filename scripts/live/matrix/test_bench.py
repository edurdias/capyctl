#!/usr/bin/env python3
"""Fake-server tests for bench.py (M80). Standard library only.

    python3 scripts/live/matrix/test_bench.py

A local fake streaming server stands in for the router. These tests check the
harness's parsing and arithmetic only; they are not evidence for any row.
"""

import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ.setdefault("MLLM_API_KEY", "fake-key-for-tests")
import bench  # noqa: E402

TIMING = {"deployment": "d", "engine": "vllm", "instance": 0, "generation": 3, "queue_wait_ms": 0.0,
          "selection_ms": 0.2, "lease_grant_ms": 1.5, "pre_forward_ms": 2.0, "time_to_first_byte_ms": 30.0,
          "time_to_first_content_ms": 31.0, "time_to_last_chunk_ms": 60.0, "total_ms": 61.0,
          "activation_wait_ms": None}


class FakeRouter(http.server.BaseHTTPRequestHandler):
    timing_comment = True

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("x-mllm-timing", json.dumps({"selection_ms": 0.2}))
        self.send_header("x-mllm-instance", "0")
        self.end_headers()
        events = [{"choices": [{"delta": {"role": "assistant"}}]}]
        events += [{"choices": [{"delta": {"content": f"w{i} "}}]} for i in range(body["max_tokens"])]
        events.append({"choices": [{"delta": {}, "finish_reason": "length"}],
                       "usage": {"prompt_tokens": 100, "completion_tokens": body["max_tokens"]}})
        for event in events:
            self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")
            self.wfile.flush()
        if self.timing_comment:
            self.wfile.write(b": x-mllm-timing " + json.dumps(TIMING).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")


def view(router_first_content, ingress_first_byte, engine_ttft, count):
    """A status view with a `latency` field shaped as the server reports it."""
    def series(name, tier, source, mean):
        return {"name": name, "tier": tier, "source": source, "count": count, "sum_seconds": mean * count,
                "buckets": [{"le": 0.05, "count": count}]}
    return {"id": "d", "latency": [{"instance": 0, "generation": 3, "engine": "vllm", "host_id": "h", "series": [
        series("router_time_to_first_content", "router", "mllm", router_first_content),
        series("router_upstream_first_byte", "router", "mllm", ingress_first_byte + 0.004),
        series("router_pre_forward", "router", "mllm", 0.002),
        series("ingress_time_to_first_byte", "ingress", "mllm", ingress_first_byte),
        series("engine_time_to_first_token", "engine", "engine", engine_ttft),
    ]}]}


class BenchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeRouter)
        cls.base = f"http://127.0.0.1:{cls.server.server_address[1]}"
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def test_timing_comment_is_kept_and_not_malformed(self):
        rec = bench.stream_chat("m", "hi", max_tokens=4, ignore_eos=True, base=self.base, timeout=10)
        self.assertEqual(rec["status"], 200)
        self.assertEqual(rec["sse_malformed"], 0)
        self.assertTrue(rec["sse_done"])
        self.assertEqual(rec["mllm_timing"]["total_ms"], 61.0)
        m = bench.derive(rec, 100)
        self.assertTrue(m["ok"])
        self.assertEqual(m["completion_tokens"], 4)
        summary = bench.mllm_timing_summary([rec, rec])
        self.assertAlmostEqual(summary["time_to_first_content_s"]["p50"], 0.031)
        self.assertEqual(summary["time_to_first_content_s"]["n"], 2)
        cell = bench.summarize_cell([dict(rec, words=90, t_send_unix_ms=rec["t_send_unix_ms"])], 1.0)
        self.assertEqual(cell["answering"], ["x-mllm-instance=0"], "per-request timing is not an answering id")

    def test_stream_without_comment_is_unchanged(self):
        FakeRouter.timing_comment = False
        try:
            rec = bench.stream_chat("m", "hi", max_tokens=2, ignore_eos=False, base=self.base, timeout=10)
        finally:
            FakeRouter.timing_comment = True
        self.assertNotIn("mllm_timing", rec)
        self.assertTrue(bench.derive(rec, 10)["ok"])
        self.assertEqual(bench.mllm_timing_summary([rec]), {})

    def test_latency_window_and_path_overhead(self):
        before = view(0.020, 0.010, 0.008, 2)
        # Two more requests in the window: all at the new means.
        after = view(0.0, 0.0, 0.0, 4)
        for inst_b, inst_a in zip(before["latency"], after["latency"]):
            for sb, sa in zip(inst_b["series"], inst_a["series"]):
                sa["sum_seconds"] = sb["sum_seconds"] + {"router_time_to_first_content": 0.030,
                                                         "router_upstream_first_byte": 0.024,
                                                         "router_pre_forward": 0.004,
                                                         "ingress_time_to_first_byte": 0.020,
                                                         "engine_time_to_first_token": 0.016}[sa["name"]]
        series = bench.latencydelta(before, after)
        self.assertEqual(series["engine_time_to_first_token"]["count"], 2)
        self.assertEqual(series["engine_time_to_first_token"]["source"], "engine")
        self.assertAlmostEqual(series["router_time_to_first_content"]["mean"], 0.015)
        self.assertIsNotNone(series["ingress_time_to_first_byte"]["p50_bucketed"])
        o = bench.path_overhead(series, {"ttft_s": {"mean": 0.020}, "ttlt_s": {"mean": 0.5}})
        self.assertAlmostEqual(o["client_to_router_ttft_mean_s"], 0.005)
        self.assertAlmostEqual(o["router_to_ingress_first_byte_mean_s"], 0.002)
        self.assertAlmostEqual(o["ingress_to_engine_ttft_mean_s"], 0.002)
        self.assertAlmostEqual(o["path_overhead_ttft_mean_s"], 0.012)
        self.assertIsNone(o["path_overhead_e2e_mean_s"], "no engine e2e series in this window")
        # The API report shape reads the same as a status view.
        api = {"deployments": [{"deployment_id": "d", "instances": after["latency"]}]}
        self.assertEqual(bench.latencydelta(before, api), series)
        self.assertEqual(bench.latencydelta({}, {}), {})

    def test_report_includes_the_mllm_side(self):
        with tempfile.TemporaryDirectory() as evid:
            os.makedirs(os.path.join(evid, "cells"))
            os.makedirs(os.path.join(evid, "latency"))
            rec = bench.stream_chat("m", "hi", max_tokens=3, ignore_eos=True, base=self.base, timeout=10)
            rec.update({"words": 90})
            cell = {"cell": "L128-C1", "concurrency": 1, "prompt_tokens_requested": 128}
            cell.update(bench.summarize_cell([rec], 1.0))
            bench.write_json(os.path.join(evid, "cells", "L128-C1.json"), cell)
            bench.write_json(os.path.join(evid, "latency", "L128-C1.before.json"), view(0.01, 0.01, 0.01, 1))
            bench.write_json(os.path.join(evid, "latency", "L128-C1.after.json"), view(0.01, 0.01, 0.005, 3))
            out = subprocess.run([sys.executable, os.path.join(HERE, "bench.py"), "report", "--evid", evid,
                                  "--fixture", "v92-4"], capture_output=True, text=True, check=True).stdout
            self.assertIn("Path overhead from the mllm latency view", out)
            report = bench.read_json(os.path.join(evid, "bench.json"))
            side = report["cells"][0]["mllm_side"]
            self.assertEqual(side["overhead"]["engine_ttft_source"], "engine")
            self.assertEqual(side["series"]["engine_time_to_first_token"]["count"], 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
