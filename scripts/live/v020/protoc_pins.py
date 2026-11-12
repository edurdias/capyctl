#!/usr/bin/env python3
"""Install checksum-pinned protoc privately under a live session; print its path."""
import argparse
import hashlib
from pathlib import Path, PurePosixPath
import platform
import shutil
import stat
import subprocess
import tempfile
import urllib.request
import zipfile


# Cargo.toml uses prost/tonic 0.13 and capyctl-protocol/build.rs invokes
# tonic-build 0.13 without experimental flags. management.proto uses proto3
# optional fields, requiring protoc >= 3.15. Pin a stable compatible release.
VERSION = '29.3'
PINS = {
    'x86_64': ('x86_64', '3e866620c5be27664f3d2fa2d656b5f3e09b5152b42f1bedbf427b333e90021a'),
    'aarch64': ('aarch_64', '6427349140e01f06e049e707a58709a4f221ae73ab9a0425bc4a00c8d0e1ab32'),
}


def install(root):
    if platform.system() != 'Linux' or platform.machine() not in PINS:
        raise ValueError('pinned protoc supports Linux x86_64 and aarch64 only')
    architecture, expected = PINS[platform.machine()]
    tools = root.resolve() / 'protoc-tools'
    tools.mkdir(parents=True, exist_ok=True, mode=0o700)
    destination = Path(tempfile.mkdtemp(prefix=f'protoc-{VERSION}-', dir=tools))
    try:
        archive = destination / 'download.zip'
        url = (f'https://github.com/protocolbuffers/protobuf/releases/download/v{VERSION}/'
               f'protoc-{VERSION}-linux-{architecture}.zip')
        digest = hashlib.sha256()
        with urllib.request.urlopen(url, timeout=120) as response, archive.open('xb') as output:
            for chunk in iter(lambda: response.read(1024 * 1024), b''):
                digest.update(chunk)
                output.write(chunk)
        if digest.hexdigest() != expected:
            raise ValueError('protoc archive SHA-256 mismatch')
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                path = PurePosixPath(member.filename)
                mode = member.external_attr >> 16
                if (path.is_absolute() or '..' in path.parts or '\\' in member.filename
                        or stat.S_ISLNK(mode)):
                    raise ValueError('unsafe protoc archive member')
                # Do not delegate path handling or Unix permissions to extractall.
                if member.is_dir():
                    continue
                target = destination.joinpath(*path.parts)
                target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                with bundle.open(member) as source, target.open('xb') as output:
                    shutil.copyfileobj(source, output)
        archive.unlink()
        executable = destination / 'bin' / 'protoc'
        executable.chmod(0o700)
        result = subprocess.run([str(executable), '--version'], check=True,
                                capture_output=True, text=True, timeout=30)
        if result.stdout.strip() != f'libprotoc {VERSION}':
            raise ValueError('unexpected protoc version')
        return executable
    except BaseException:
        shutil.rmtree(destination)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    args = parser.parse_args()
    print(install(args.root))


if __name__ == '__main__':
    main()
