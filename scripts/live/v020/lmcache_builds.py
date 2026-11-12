"""Build ADR 0034's explicitly non-release qualification binaries from copies."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def prepare(source, destination, patch):
    source, destination, patch = map(Path, (source, destination, patch))
    if destination.exists():
        raise ValueError('qualification source destination already exists')
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns('target', '.git', '.worktrees'))
    subprocess.run(['patch', '--batch', '--fuzz=0', '-p1', '-i', str(patch.resolve())],
                   cwd=destination, check=True, capture_output=True, text=True)
    return dict(patch=patch.name, patch_sha256=digest(patch), source_copy=str(destination))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    args = parser.parse_args()
    here = Path(__file__).resolve().parent
    for row in ('lc12', 'lc18'):
        destination = args.root / ('qualification-' + row)
        receipt = prepare(args.source, destination, here / ('lmcache-' + row + '.patch'))
        target = destination / 'target'
        subprocess.run(['cargo', 'build', '--locked', '--release', '-p', 'capyctl-cli', '--bin', 'capyctl',
                        '--target-dir', str(target)], cwd=destination, check=True)
        binary = args.root / 'bin' / ('capyctl-' + row)
        shutil.copy2(target / 'release/capyctl', binary)
        receipt.update(binary=str(binary), binary_sha256=digest(binary),
                       qualification_only=True, release_binary_replaced=False,
                       source_revision=(args.root / 'source-revision.txt').read_text().strip())
        (args.root / 'evidence' / ('lmcache-' + row + '-build.json')).write_text(json.dumps(receipt,indent=2)+'\n')


if __name__ == '__main__':
    main()
