# Runtime preflight

## Selected SGLang source preflight

`sglang_source_preflight.verify_sglang_sources(root)` checks seven fixed Python
sources under the installed `sglang/srt` directory against commit
`fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`. The inventory covers server arguments,
authentication, the saver adapter, scheduler, weight updater, engine startup, and
HTTP startup. `revalidate_sglang_sources(previous)` repeats the checks and rejects
even byte-identical inode replacement. Roots stay out of observation repr; errors
are closed categories. No engine module is imported.

Every directory ancestor and source leaf must be root/service-owned and have no
group/world write permission. Symlinks and nonregular sources are rejected;
individual files are bounded to 16 MiB. Tests use private temporary directories
beneath the service home because `/tmp` is not a protected installation ancestor.

These seven files are not the complete Python import graph or compiled runtime.
The check neither attests the whole installed wheel nor proves the effective
ServerArgs mapping, physical GPU identity, real saver, worker enrollment, or live
compatibility. Native startup remains closed; production composition must select
the protected installed package root and consume/revalidate this observation
alongside the remaining gates. Same-service-user mutation after observation is
outside its guarantee.

## Checkpoint preflight

This module performs a bounded observation of one fixed checkpoint contract. It
does not qualify an engine or authorize a launch.

## Public API

```python
from runtime.checkpoint_preflight import (
    CheckpointPreflightError, verify_checkpoint, revalidate_checkpoint,
)

verified = verify_checkpoint("/path/to/checkpoint")
# Immediately before native loading (and before any protected wrapper/effect):
verified = revalidate_checkpoint(verified)
```

`verify_checkpoint(root: str) -> VerifiedCheckpoint` checks the compiled-in
manifest for `Qwen/Qwen3-4B-Instruct-2507` at revision
`cdbee75f17c01a7cc42f958dc650907174af0554`. It validates the expected artifact
bytes, JSON, safetensors geometry and storage layout, while retaining only
scoped immutable facts. `revalidate_checkpoint(previous: VerifiedCheckpoint) ->
VerifiedCheckpoint` repeats the full check and requires the same root and file
identities. Byte-identical replacement at a different inode is therefore
rejected.

The compiled-in manifest is fixed: callers cannot provide expected hashes,
geometry, revision, or a test mode. The verifier uses descriptor-safe,
bounded I/O and warms only the filesystem cache as a consequence of reading;
it does not import or load models, tensors, or engines. It performs no network
access, download, checkpoint mutation, engine initialization, or routing side
effect.

The supported root is a Linux-only, absolute, normalized path: it has no
trailing or repeated separators and no `.` or `..` components. Symlink path
components and leaves, and non-regular artifacts, are rejected. Reads are
bounded to 1 MiB chunks and a 16 MiB limit for ancillary files. The verifier
detects ordinary replacement and concurrent modification during its check, but
does not establish immutable storage or close the interval after final
revalidation if a later engine reopens pathnames. Same-user malicious writers
remain outside this module's guarantee; a downstream launcher must retain
trusted runtime and filesystem assumptions.

The index's pinned advisory `total_size` is checked against the pinned payload
relationship. This exact checkpoint has a known 655,360-byte discrepancy
between the advisory total and tensor payload; that value is part of the fixed
contract and is not a generic corruption tolerance.

## Normal and failure usage

```python
try:
    verified = verify_checkpoint("/path/to/checkpoint")
    verified = revalidate_checkpoint(verified)
except CheckpointPreflightError as error:
    print(error.code)
```

Failures expose only one closed error-code set:

`invalid_root`, `unsupported_platform`, `unsafe_file`, `artifact_missing`,
`artifact_changed`, `artifact_mismatch`, `invalid_json`, `invalid_storage`,
`unsupported_geometry`, and `io_error`.

Synthetic CPU fixtures in `runtime/tests/test_checkpoint_preflight.py` exercise
the verifier's safety and parsing seams, but they are not evidence that the
real pinned model is present or loadable. The result is only an observation of
checkpoint files at that moment. Future native composition must revalidate
immediately before loading, before any protected wrapper/effect. A successful
result grants no qualification, admission, runtime provenance, engine,
phase-budget, or F2 authority.
