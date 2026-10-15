"""Tests for rdma_counters.py (multi-node rows MN1-MN9). Standard library only.

    python3 -m unittest discover -s scripts/live/matrix/tests

A temporary directory stands in for /sys/class/infiniband. These tests check the
helper's reading and arithmetic only; they are not evidence for any row.
"""

import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import rdma_counters  # noqa: E402


def write(path, value):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as handle:
        handle.write(f"{value}\n")


def tree(root, ports, operations=None):
    for dev, port, tx, rx in ports:
        counters = os.path.join(root, dev, "ports", port, "counters")
        write(os.path.join(counters, "port_xmit_data"), tx)
        write(os.path.join(counters, "port_rcv_data"), rx)
    for (dev, port), values in (operations or {}).items():
        for name, value in values.items():
            write(os.path.join(root, dev, "ports", port, "hw_counters", name), value)


class CounterTests(unittest.TestCase):
    def test_sums_every_port(self):
        with tempfile.TemporaryDirectory() as root:
            tree(root, [("mlx5_0", "1", "10", "1"), ("mlx5_1", "1", "5", "1")])
            out = rdma_counters.read(root)
            self.assertTrue(out["present"])
            self.assertEqual(out["devices"]["mlx5_0"]["xmit"], 10)
            self.assertEqual(out["devices"]["mlx5_1"]["rcv"], 1)
            self.assertEqual(out["total"]["xmit"], 15)
            self.assertEqual(out["total"]["rcv"], 2)

    def test_sums_ports_within_a_device(self):
        with tempfile.TemporaryDirectory() as root:
            tree(root, [("mlx5_0", "1", "7", "3"), ("mlx5_0", "2", "4", "6")])
            out = rdma_counters.read(root)
            self.assertEqual(out["devices"]["mlx5_0"]["xmit"], 11)
            self.assertEqual(out["devices"]["mlx5_0"]["rcv"], 9)
            self.assertEqual(out["devices"]["mlx5_0"]["ports"]["2"], {"xmit": 4, "rcv": 6})

    def test_missing_tree(self):
        self.assertEqual(rdma_counters.read("/nonexistent"), {"present": False})

    def test_unreadable_counter_is_skipped(self):
        with tempfile.TemporaryDirectory() as root:
            tree(root, [("mlx5_0", "1", "10", "2")])
            write(os.path.join(root, "mlx5_0", "ports", "1", "counters", "port_xmit_data"), "n/a")
            out = rdma_counters.read(root)
            self.assertEqual(out["devices"]["mlx5_0"]["xmit"], 0)
            self.assertEqual(out["devices"]["mlx5_0"]["rcv"], 2)

    def test_operation_counters_are_summed_when_present(self):
        with tempfile.TemporaryDirectory() as root:
            tree(root, [("mlx5_0", "1", "10", "2")],
                 {("mlx5_0", "1"): {"rx_write_requests": 4, "rx_read_requests": 1}})
            out = rdma_counters.read(root)
            self.assertEqual(out["devices"]["mlx5_0"]["operations"],
                             {"rx_write_requests": 4, "rx_read_requests": 1})
            self.assertEqual(out["total"]["operations"]["rx_write_requests"], 4)


class DeltaTests(unittest.TestCase):
    def readings(self, before_ports, after_ports, before_ops=None, after_ops=None):
        with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
            tree(a, before_ports, before_ops)
            tree(b, after_ports, after_ops)
            return rdma_counters.read(a), rdma_counters.read(b)

    def test_devices_that_rose_both_ways_carried_traffic(self):
        before, after = self.readings([("mlx5_0", "1", 10, 10), ("mlx5_1", "1", 5, 5)],
                                      [("mlx5_0", "1", 30, 25), ("mlx5_1", "1", 9, 5)])
        out = rdma_counters.delta(before, after)
        self.assertEqual(out["devices"]["mlx5_0"], {"xmit": 20, "rcv": 15})
        self.assertEqual(out["carried"], ["mlx5_0"])
        self.assertFalse(out["operations_known"])
        self.assertFalse(out["rdma_operations"])

    def test_rdma_operations_rise(self):
        before, after = self.readings([("mlx5_0", "1", 1, 1)], [("mlx5_0", "1", 2, 2)],
                                      {("mlx5_0", "1"): {"rx_write_requests": 3}},
                                      {("mlx5_0", "1"): {"rx_write_requests": 9}})
        out = rdma_counters.delta(before, after)
        self.assertEqual(out["devices"]["mlx5_0"]["operations"], {"rx_write_requests": 6})
        self.assertTrue(out["operations_known"])
        self.assertTrue(out["rdma_operations"])

    def test_absent_reading(self):
        out = rdma_counters.delta({"present": False}, {"present": False})
        self.assertEqual(out, {"present": False, "devices": {}, "carried": [], "rdma_operations": False})

    def test_cli_delta(self):
        before, after = self.readings([("mlx5_0", "1", 1, 1)], [("mlx5_0", "1", 4, 6)])
        with tempfile.TemporaryDirectory() as d:
            paths = []
            for name, reading in (("before", before), ("after", after)):
                path = os.path.join(d, name + ".json")
                with open(path, "w") as handle:
                    json.dump(reading, handle)
                paths.append(path)
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(rdma_counters.main(["delta", *paths]), 0)
            self.assertEqual(json.loads(out.getvalue())["carried"], ["mlx5_0"])


if __name__ == "__main__":
    unittest.main()
