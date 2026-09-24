#!/usr/bin/env python3
"""What one owned engine process was actually given (rows M73; SPEC section 13.2).

Runs on the Spark. The identity is (pid, start ticks, boot id) from the server's
recorded launch identities (ledger.py owned), checked exactly as signal_owned.py
checks it, so a reused pid is never read. It prints one JSON line:

  outcome        alive | absent | refused_boot_mismatch | refused_identity_mismatch
  argv           /proc/<pid>/cmdline as the kernel recorded it at exec, with the value of
                 any credential-shaped option replaced by <redacted>
  credential_in_argv   true when a credential-shaped option appeared in argv at all
                 (SPEC section 3: credentials never ride argv)
  env            only the variables named with --env, read from /proc/<pid>/environ;
                 a credential-shaped name is refused rather than read

The launch plan says what the engine should have been given; this reads what it
was given, which is the difference the removed live suites checked (L1, SGL1).

usage: proc_probe.py --pid P --ticks T --boot B [--env NAME ...]
"""

import argparse
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from signal_owned import live_identity  # noqa: E402

SECRET = re.compile(r"(key|token|secret|password|credential)", re.IGNORECASE)


def redact(argv):
    # A credential passed by descriptor (`--inference-credential-fd 20`) carries
    # only a descriptor number on argv, which is the SPEC section 3 shape, not a
    # credential; it is kept visible and not counted (found live, M73 s17-4).
    out, hidden, i = [], False, 0
    while i < len(argv):
        arg = argv[i]
        name = arg.split("=", 1)[0]
        if arg.startswith("--") and SECRET.search(name):
            value = arg.split("=", 1)[1] if "=" in arg else (argv[i + 1] if i + 1 < len(argv) else "")
            if name.endswith("-fd") and value.isdigit():
                out.append(arg)
                if "=" not in arg and i + 1 < len(argv):
                    out.append(argv[i + 1])
                    i += 1
                i += 1
                continue
            hidden = True
            if "=" in arg:
                out.append(name + "=<redacted>")
            else:
                out.append(arg)
                if i + 1 < len(argv):
                    out.append("<redacted>")
                    i += 1
            i += 1
            continue
        out.append(arg)
        i += 1
    return out, hidden


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--ticks", type=int, required=True)
    parser.add_argument("--boot", required=True)
    parser.add_argument("--env", action="append", default=[])
    args = parser.parse_args()
    for name in args.env:
        if SECRET.search(name):
            sys.exit(f"refusing to read a credential-shaped variable {name}")
    with open("/proc/sys/kernel/random/boot_id") as handle:
        boot = handle.read().strip()
    result = {"pid": args.pid, "ticks": args.ticks, "boot": args.boot}
    live = live_identity(args.pid)
    if boot != args.boot:
        result["outcome"] = "refused_boot_mismatch"
    elif live is None:
        result["outcome"] = "absent"
    elif live["ticks"] != args.ticks:
        result["outcome"] = "refused_identity_mismatch"
    else:
        result["outcome"] = "alive"
        with open(f"/proc/{args.pid}/cmdline", "rb") as handle:
            argv = [a.decode(errors="replace") for a in handle.read().split(b"\0") if a]
        result["argv"], result["credential_in_argv"] = redact(argv)
        env = {}
        try:
            with open(f"/proc/{args.pid}/environ", "rb") as handle:
                entries = handle.read().split(b"\0")
        except PermissionError:
            entries = []
            result["env_error"] = "environ unreadable"
        wanted = set(args.env)
        for entry in entries:
            name, _, value = entry.decode(errors="replace").partition("=")
            if name in wanted:
                env[name] = value
        result["env"] = env
    print(json.dumps(result, sort_keys=True))
    return 0 if result["outcome"] == "alive" else 1


if __name__ == "__main__":
    sys.exit(main())
