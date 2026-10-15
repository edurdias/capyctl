#!/usr/bin/env python3
"""RDMA port counters of one host, read-only (multi-node rows MN1-MN9, ADR 0028).

Runs on the host. Sums `port_xmit_data` and `port_rcv_data` over every port of
every device under /sys/class/infiniband/<device>/ports/<port>/counters/ and
prints JSON. A host without that tree prints {"present": false}. Where the
driver exposes them (mlx5), the RDMA operation counters under
ports/<port>/hw_counters/ are summed too: on a RoCE port `port_*_data` also
counts ordinary Ethernet traffic, so only a rise in the operation counters shows
that RDMA itself carried data. Nothing is ever written.

  rdma_counters.py [--root DIR]            the counters now
  rdma_counters.py delta BEFORE AFTER      per-device rise between two readings

`port_xmit_data` and `port_rcv_data` are the kernel's raw values (on mlx5,
octets divided by four); the rows compare them, never convert them.
"""

import argparse
import json
import os
import sys

ROOT = "/sys/class/infiniband"
PORT = {"port_xmit_data": "xmit", "port_rcv_data": "rcv"}
# Requests a responder served for a peer (RDMA write, read, atomic): non-zero
# only when RDMA verbs moved data through the port.
OPERATIONS = ("rx_write_requests", "rx_read_requests", "rx_atomic_requests")


def _value(path):
    try:
        with open(path) as handle:
            return int(handle.read().strip())
    except (OSError, ValueError):
        return None


def read(root=ROOT):
    """The counters under `root`, or {"present": False} when it is absent."""
    if not os.path.isdir(root):
        return {"present": False}
    devices = {}
    total = {"xmit": 0, "rcv": 0}
    operations_total = {}
    for device in sorted(os.listdir(root)):
        ports_dir = os.path.join(root, device, "ports")
        if not os.path.isdir(ports_dir):
            continue
        entry = {"xmit": 0, "rcv": 0, "ports": {}}
        operations = {}
        for port in sorted(os.listdir(ports_dir)):
            counters = os.path.join(ports_dir, port, "counters")
            values = {}
            for name, key in PORT.items():
                value = _value(os.path.join(counters, name))
                if value is not None:
                    values[key] = value
                    entry[key] += value
            hw = os.path.join(ports_dir, port, "hw_counters")
            for name in OPERATIONS:
                value = _value(os.path.join(hw, name))
                if value is not None:
                    operations[name] = operations.get(name, 0) + value
            entry["ports"][port] = values
        if operations:
            entry["operations"] = operations
            for name, value in operations.items():
                operations_total[name] = operations_total.get(name, 0) + value
        devices[device] = entry
        total["xmit"] += entry["xmit"]
        total["rcv"] += entry["rcv"]
    if operations_total:
        total["operations"] = operations_total
    return {"present": True, "devices": devices, "total": total}


def delta(before, after):
    """Per-device rise from `before` to `after`, and the devices that carried traffic."""
    if not before.get("present") or not after.get("present"):
        return {"present": False, "devices": {}, "carried": [], "rdma_operations": False}
    devices = {}
    for name, now in after["devices"].items():
        then = before["devices"].get(name, {})
        rise = {key: now.get(key, 0) - then.get(key, 0) for key in ("xmit", "rcv")}
        ops_now, ops_then = now.get("operations", {}), then.get("operations", {})
        if ops_now:
            rise["operations"] = {op: ops_now[op] - ops_then.get(op, 0) for op in ops_now}
        devices[name] = rise
    carried = sorted(name for name, rise in devices.items() if rise["xmit"] > 0 and rise["rcv"] > 0)
    rdma = any(value > 0 for rise in devices.values() for value in rise.get("operations", {}).values())
    operations_known = any("operations" in rise for rise in devices.values())
    return {"present": True, "devices": devices, "carried": carried,
            "operations_known": operations_known, "rdma_operations": rdma}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("cmd", nargs="?", choices=["read", "delta"], default="read")
    parser.add_argument("files", nargs="*")
    parser.add_argument("--root", default=ROOT)
    args = parser.parse_args(argv)
    if args.cmd == "delta":
        if len(args.files) != 2:
            parser.error("delta takes BEFORE and AFTER")
        with open(args.files[0]) as a, open(args.files[1]) as b:
            result = delta(json.load(a), json.load(b))
    else:
        result = read(args.root)
    json.dump(result, sys.stdout, sort_keys=True)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
