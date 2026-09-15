# F2 Native Source Preflight Implementation Plan

**Goal:** Verify the selected installed SGLang control/startup Python files against the reviewed source pin without importing the engine.

**Architecture:** A standard-library verifier observes a fixed source inventory under one trusted package root, retaining file and directory identities for revalidation. Reuse the existing descriptor-safe checkpoint observation primitive per source directory. This is source observation only; the native launch denial remains intact until source mapping, physical placement, logging, saver, and enrollment gates are implemented.

**Tech Stack:** Python standard library and unittest; existing checkpoint preflight primitives.

**Spec:** [F2B](2026-09-12-f2b-sglang-adapter.md), Task 1; [native prerequisites](../../runbooks/f2-native-startup-prerequisites.md).

## Global Constraints

- Source pin is `fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`; no caller-selected expected hashes.
- No engine imports, GPU access, package installation, launch, credential access, or qualification.
- Selected Python source hashes do not attest the complete wheel or compiled binaries.
- Protect all observed directories/files against non-service writes; reject links and nonregular files.
- Preserve the unrelated CLI interactive test and A2d task-2 report.

### Task 1: Fixed source inventory and identity-preserving revalidation

**Files:** Create `runtime/sglang_source_preflight.py`, `runtime/tests/test_sglang_source_preflight.py`; update `runtime/README.md`.

**Interfaces:**
- `verify_sglang_sources(root: str) -> VerifiedSglangSources` takes the absolute installed `sglang/srt` directory, never an import/module object.
- `revalidate_sglang_sources(previous: VerifiedSglangSources) -> VerifiedSglangSources` repeats bytes and identities, rejecting replacement even if bytes match.
- `SourcePreflightError.code` exposes closed sanitized codes; private roots are excluded from repr.
- Keep `_observe_sources(root, inventory)` private for synthetic CPU fixtures. Production functions use only the compiled-in tuple.

- [ ] Add RED tests with a small synthetic nested source inventory. Test matching hashes, mismatch, missing leaf, symlinked intermediate directory, symlink/FIFO leaf, oversized file, group/world write permissions, changed inode, malformed root, and import safety.

```python
def test_same_bytes_new_inode_rejected(self):
    before = self.observe()
    self.leaf.rename(self.leaf.with_suffix('.old'))
    self.leaf.write_bytes(self.payload)
    after = self.observe()
    self.assertNotEqual(before, after)
```

- [ ] Run `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s runtime/tests -p test_sglang_source_preflight.py -v`; require failure from missing verifier.
- [ ] Implement immutable source observations and compile these seven reviewed hashes:

```python
_SOURCES = (
    ('server_args.py', 'e04556de6d99ba8a76b91fffa49aa70ea9d65cd99f09d0da5d4cfe1228e28500'),
    ('utils/auth.py', '016734a0263cbc2bd6657481ab11535f1cac073efb7eed4bcdbf66f81dfd3ef7'),
    ('utils/torch_memory_saver_adapter.py', '196266b5ac6c7a805b36c9953fcdd9cafb0dcc3ac7a9e25aae6edf34a8c6fba9'),
    ('managers/scheduler.py', '159837693ec244d2dfb482d5788b8f4fa934c780505609c90a728939abe89815'),
    ('managers/scheduler_components/weight_updater.py', '60cc68d85a9399be91c681db68c3a57a4697354912483e3c7e61127caa7bc977'),
    ('entrypoints/engine.py', '3fc01012d5e06050767573a31ea6a5b07447e88136d23cb7fd8454743dac1db4'),
    ('entrypoints/http_server.py', '5cdce94aaf3a446ac43e5a8e46961cbafdca9de7f0234404d285ed47ef5274d6'),
)
```

- [ ] Group inventory by parent directory and call checkpoint `_observe` with bare leaf names, not nested relative names. Check each full directory chain and source leaf for root/service ownership and no group/world writes before observing, then repeat permission checks after observation. `_observe` supplies hash bounds, no-follow directory opens, final identity checks, and a 16 MiB per-file ceiling.

```python
with ExitStack() as stack:
    chain = _open_chain(parent, stack)
    for fd in chain:
        info = os.fstat(fd)
        if info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
            raise SourcePreflightError('unsafe_file')
```

- [ ] Translate existing preflight errors to the sanitized source error type; never render caught paths. Revalidation requires exact observations and source revision. Neither API accepts expected hashes or supplies launch authority.
- [ ] Run focused tests and the complete runtime suite with `TMS_SOURCE_ARCHIVE=<saver-source-archive>`; require nonzero test count and GREEN.
- [ ] Document the seven-file boundary and unchanged native denial. Commit only the plan, module, tests, and runtime README.

## Coverage and remaining gates

This task covers installed selected-source byte/identity checks only. It does not close F2B Task 1. Effective ServerArgs mapping, protected installation identity, physical GPU resolution, secret-safe logging/plugin policy, real saver guards, and worker enrollment remain in the parent plan and prerequisite ledger. No new owner decision is required for this local deterministic slice.
