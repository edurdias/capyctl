#!/usr/bin/env python3
"""Run a qualification command and delete its explicitly named Shadeform rental.

Launch under a user systemd service, not an agent process. Provider auto-deletion
must also be configured: no local supervisor survives a powered-off laptop.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import uuid


def record(path, value):
    temporary = path.with_suffix(".new")
    with temporary.open("w") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def shade(*args):
    result = subprocess.run(
        ["shade", "instances", *args, "--agent-mode=false", "--no-interactive",
         "--output-format", "json", "--timeout", "30s"],
        capture_output=True, text=True, timeout=40, check=True,
    )
    return json.loads(result.stdout) if result.stdout.strip() else {}


def delete_rental(instance, output):
    # Record only allowlisted provider fields: never dump authentication output.
    receipt = {"instance": instance, "deletion_requested": False, "status": "unknown"}
    for attempt in range(1, 7):
        try:
            current = shade("get", instance)
            receipt.update({key: current.get(key) for key in
                            ("status", "cost_estimate", "deleted_at")})
            if current.get("status") == "deleted":
                record(output / "rental-deletion.json", receipt)
                return True
            if current.get("status") != "deleting":
                shade("delete", instance)
                receipt["deletion_requested"] = True
        except (subprocess.SubprocessError, OSError, ValueError) as error:
            receipt["error_type"] = type(error).__name__
        receipt["attempt"] = attempt
        record(output / "rental-deletion.json", receipt)
        time.sleep(10)
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--instance", required=True, type=lambda value: str(uuid.UUID(value)))
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--max-seconds", type=int, default=28800)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or args.max_seconds <= 0:
        parser.error("a command and positive --max-seconds are required")
    args.out.mkdir(parents=True, exist_ok=True, mode=0o700)
    stopping = False

    def stop(_signal, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    state = {"instance": args.instance, "started": datetime.datetime.now(datetime.timezone.utc).isoformat(),
             "command_status": None, "reason": "running"}
    child = None
    code = 1
    try:
        record(args.out / "supervisor.json", state)
        with (args.out / "session.log").open("ab", buffering=0) as log:
            child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            deadline = time.monotonic() + args.max_seconds
            while child.poll() is None and not stopping and time.monotonic() < deadline:
                time.sleep(1)
            if child.poll() is None:
                state["reason"] = "signal" if stopping else "deadline"
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=120)  # let the runner make its final evidence copy
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
                code = 124
            else:
                state["reason"] = "command_finished"
                code = child.returncode
            state["command_status"] = child.returncode
    finally:
        state["ended"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        try:
            record(args.out / "supervisor.json", state)
        finally:
            if not delete_rental(args.instance, args.out):
                print("Rental deletion not confirmed; inspect rental-deletion.json and provider auto-delete", flush=True)
                code = 1
    return code


if __name__ == "__main__":
    raise SystemExit(main())
