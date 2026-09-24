#!/usr/bin/env python3
"""The one bounded external memory allocation of decision D6 (M21).

Runs on a Spark with the system python3. Allocates at most 40 GiB of anonymous
memory, touches every page so MemAvailable really drops, holds it for at most
--seconds, then exits; the kernel reclaims everything at exit. SIGTERM, SIGINT
and SIGHUP end it early through the same path. fault.sh also wraps it in
`timeout` so a wedged interpreter is still killed.

usage: memhog.py --gib N --seconds S --pidfile FILE
"""

import argparse
import mmap
import os
import signal
import sys
import time

CEILING_GIB = 40
MAX_SECONDS = 1800
PAGE = mmap.PAGESIZE


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--gib", type=float, required=True)
    parser.add_argument("--seconds", type=int, required=True)
    parser.add_argument("--pidfile", required=True)
    args = parser.parse_args()
    if not 0 < args.gib <= CEILING_GIB:
        sys.exit(f"--gib must be in (0, {CEILING_GIB}]")
    if not 0 < args.seconds <= MAX_SECONDS:
        sys.exit(f"--seconds must be in (0, {MAX_SECONDS}]")

    stop = {"now": False}

    def on_signal(signum, _frame):
        stop["now"] = True

    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, on_signal)

    with open(f"/proc/{os.getpid()}/stat") as handle:
        ticks = handle.read().rsplit(")", 1)[1].split()[19]
    with open("/proc/sys/kernel/random/boot_id") as handle:
        boot = handle.read().strip()
    tmp = args.pidfile + ".tmp"
    with open(tmp, "w") as handle:
        handle.write(f"{os.getpid()} {ticks} {boot}\n")
    os.replace(tmp, args.pidfile)

    size = int(args.gib * (1 << 30)) // PAGE * PAGE
    region = mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
    deadline = time.monotonic() + args.seconds
    touched = 0
    try:
        for offset in range(0, size, PAGE):
            if stop["now"] or time.monotonic() > deadline:
                break
            region[offset] = 1
            touched += PAGE
        print(f"memhog pid={os.getpid()} holding {touched / (1 << 30):.2f} GiB", flush=True)
        while not stop["now"] and time.monotonic() < deadline:
            time.sleep(0.5)
    finally:
        region.close()
        try:
            os.unlink(args.pidfile)
        except FileNotFoundError:
            pass
        print(f"memhog pid={os.getpid()} released", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
