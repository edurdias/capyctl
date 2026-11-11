#!/usr/bin/env python3
"""Clean only recorded session processes and paths; never signal by process name."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time


def identity(pid):
    try:
        text = (Path('/proc') / str(pid) / 'stat').read_text()
        fields = text[text.rfind(')') + 2:].split()
        if fields[0] == 'Z':
            return None
        return dict(pid=pid, start_ticks=int(fields[19]), boot_id=Path('/proc/sys/kernel/random/boot_id').read_text().strip())
    except (FileNotFoundError, ProcessLookupError):
        return None


def matches(record):
    return identity(record['pid']) == {key: record[key] for key in ('pid', 'start_ticks', 'boot_id')}


def gpu_pids():
    result = subprocess.run(['nvidia-smi', '--query-compute-apps=pid', '--format=csv,noheader,nounits'],
                            capture_output=True, text=True, timeout=20, check=True)
    return {int(line) for line in result.stdout.splitlines() if line.strip().isdigit()}


def down(root):
    runner = root / 'runner.json'
    if runner.exists():
        record = json.loads(runner.read_text())
        if record['pid'] != os.getpid() and matches(record):
            os.kill(record['pid'], signal.SIGTERM)
            deadline = time.monotonic() + 180
            while matches(record) and time.monotonic() < deadline:
                time.sleep(1)
            if matches(record):
                raise RuntimeError('runner did not stop; cleanup/purge refused')
    records = root / 'processes.jsonl'
    if records.exists():
        for line in reversed(records.read_text().splitlines()):
            record = json.loads(line)
            if matches(record):
                os.kill(record['pid'], signal.SIGTERM)
                deadline = time.monotonic() + 30
                while matches(record) and time.monotonic() < deadline:
                    time.sleep(.25)
                if matches(record):
                    raise RuntimeError('owned role did not stop; purge refused')
    before = root / 'gpu-before.json'
    if before.exists():
        original = set(json.loads(before.read_text()))
        final = gpu_pids()
        evidence = root / 'evidence/final-gpu-processes.json'
        evidence.parent.mkdir(parents=True, exist_ok=True)
        evidence.write_text(json.dumps(dict(preexisting=sorted(original), final=sorted(final),
                                           only_preexisting_remain=final <= original)) + '\n')
        if final - original:
            raise RuntimeError('new GPU processes remain; purge refused')
    marker = Path.home() / 'LIVE-SESSION-IN-USE.txt'
    if marker.exists() and marker.read_text().strip() == str(root):
        marker.unlink()


def begin(root):
    """Retain earlier attempts while giving a resumed setup fresh role state."""
    if not (root / 'completion.json').exists():
        return
    archive = root / 'attempts' / str(time.time_ns())
    archive.mkdir(parents=True, mode=0o700)
    evidence = root / 'evidence'
    if evidence.exists():
        evidence.rename(archive / 'evidence')
        evidence.mkdir(mode=0o700)
        for path in (archive / 'evidence').iterdir():
            if path.is_file() and not path.name.startswith(('result-', 'ISSUE', 'LC', 'EM', 'OT', 'TF', 'TR', 'P3-')):
                shutil.copy2(path, evidence / path.name)
    for name in ('state', 'export', 'completion.json', 'summary.json', 'summary.txt', 'runner.json', 'processes.jsonl'):
        path = root / name
        if path.exists():
            path.rename(archive / name)
    for path in root.glob('*.log'):
        path.rename(archive / path.name)


def purge(root):
    paths = root / 'created.txt'
    if paths.exists():
        for text in reversed(paths.read_text().splitlines()):
            path = Path(text)
            if path == root:
                continue
            home = Path.home().resolve()
            if not path.is_absolute() or path == home or home not in path.parents:
                raise RuntimeError('unsafe created path; purge refused')
            if path.is_symlink() or path.is_file():
                path.unlink(missing_ok=True)
            elif path.exists():
                shutil.rmtree(path)
    if root.name != 'capyctl-v020' or root.parent != Path.home():
        raise RuntimeError('unsafe session root; purge refused')
    shutil.rmtree(root, ignore_errors=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['begin', 'down', 'purge'])
    parser.add_argument('--root', required=True, type=Path)
    args = parser.parse_args()
    if args.command == 'down':
        down(args.root)
    elif args.command == 'purge':
        purge(args.root)
    else:
        begin(args.root)


if __name__ == '__main__':
    main()
