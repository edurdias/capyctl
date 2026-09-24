#!/usr/bin/env python3
"""Verify a Spark runtime directory before any engine launch (plan unit W2).

The protected SGLang and vLLM entries refuse a wrapper that others can write,
or whose ancestors others can write; Phase B found mllm_vllm_guard.py at 0664.
This check fails before a launch does. Standard library only; read-only.

usage: check_runtime.py <runtime_dir> [required-file ...]
"""

import hashlib
import os
import stat
import sys


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    root = os.path.realpath(argv[1])
    required = argv[2:]
    problems = []
    for name in required:
        path = os.path.join(root, name)
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            problems.append(f"missing {name}")
            continue
        if not stat.S_ISREG(info.st_mode):
            problems.append(f"not a regular file: {name}")
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d != "__pycache__"]
        for entry in [dirpath] + [os.path.join(dirpath, f) for f in filenames]:
            mode = os.lstat(entry).st_mode
            if mode & 0o022:
                problems.append(f"group/other-writable {oct(mode & 0o777)} {entry}")
    path = root
    while True:
        info = os.stat(path)
        if info.st_mode & 0o022 or info.st_uid not in (0, os.getuid()):
            problems.append(f"unsafe ancestor {oct(info.st_mode & 0o777)} uid={info.st_uid} {path}")
        if path == "/":
            break
        path = os.path.dirname(path)
    for name in required:
        path = os.path.join(root, name)
        if os.path.isfile(path):
            with open(path, "rb") as handle:
                print(hashlib.sha256(handle.read()).hexdigest(), name)
    for problem in problems:
        print("FAIL", problem)
    print("runtime", "ok" if not problems else "refused", root)
    return 0 if not problems else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
