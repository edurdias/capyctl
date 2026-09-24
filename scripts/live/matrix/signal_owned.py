#!/usr/bin/env python3
"""Signal one process named by ownership evidence, never by name (SPEC section 13.2, D6).

The identity is (pid, start ticks, boot id), taken either from the server's
recorded launch identities (ledger.py owned) or from a pid file the harness
wrote when it started a role (role_exec.sh) or the memory allocator. The
signal is sent only when the live process still has that exact identity, so a
reused pid is never hit. Signal 0 only checks.

usage: signal_owned.py (--pid P --ticks T --boot B | --pidfile F) --signal SIG
                       [--expect CMDLINE-SUBSTRING] [--group]
"""

import argparse
import json
import os
import signal
import sys


def live_identity(pid):
    try:
        with open(f"/proc/{pid}/stat") as handle:
            fields = handle.read().rsplit(")", 1)[1].split()
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            cmdline = handle.read().replace(b"\0", b" ").decode(errors="replace").strip()
    except FileNotFoundError:
        return None
    # After the comm field: state is index 0, pgrp index 2, starttime index 19.
    return {"state": fields[0], "pgid": int(fields[2]), "ticks": int(fields[19]), "cmdline": cmdline}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pid", type=int)
    parser.add_argument("--ticks", type=int)
    parser.add_argument("--boot")
    parser.add_argument("--pidfile")
    parser.add_argument("--signal", required=True)
    parser.add_argument("--expect")
    parser.add_argument("--group", action="store_true")
    args = parser.parse_args()
    if args.pidfile:
        with open(args.pidfile) as handle:
            pid, ticks, boot = handle.read().split()[:3]
        args.pid, args.ticks, args.boot = int(pid), int(ticks), boot
    if args.pid is None or args.ticks is None or not args.boot:
        parser.error("an identity (pid, ticks, boot) is required")
    if args.pid <= 1:
        sys.exit("refusing pid <= 1")
    name = args.signal.upper()
    signum = 0 if name in ("0", "CHECK") else getattr(signal, name if name.startswith("SIG") else "SIG" + name, None)
    if signum is None:
        sys.exit(f"unknown signal {args.signal}")
    with open("/proc/sys/kernel/random/boot_id") as handle:
        boot = handle.read().strip()
    result = {"pid": args.pid, "ticks": args.ticks, "boot": args.boot, "signal": args.signal, "group": args.group}
    live = live_identity(args.pid)
    if boot != args.boot:
        result["outcome"] = "refused_boot_mismatch"
    elif live is None:
        result["outcome"] = "absent"
    elif live["ticks"] != args.ticks:
        result["outcome"] = "refused_identity_mismatch"
    elif args.expect and args.expect not in live["cmdline"]:
        result["outcome"] = "refused_cmdline_mismatch"
    elif args.group and live["pgid"] != args.pid:
        result["outcome"] = "refused_not_group_leader"
    else:
        result["state_before"] = live["state"]
        if signum:
            if args.group:
                os.killpg(args.pid, signum)
            else:
                os.kill(args.pid, signum)
            result["outcome"] = "signalled"
        else:
            result["outcome"] = "alive"
    print(json.dumps(result, sort_keys=True))
    return 0 if result["outcome"] in ("signalled", "alive") else 1


if __name__ == "__main__":
    sys.exit(main())
