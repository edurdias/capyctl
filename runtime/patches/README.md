# Memory saver observer patch

This is a local, CPU-tested prerequisite for F2B, not an installed or deployable
CUDA artifact. It carries a minimal patch against `torch_memory_saver-0.0.9.post1`;
the complete upstream source is not vendored.

## Source and application

The exact source archive has SHA-256:

```text
25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43
```

The observer patch in this module has SHA-256:

```text
8afc3e68a6b11a5c0d6bb3e7ab78eb7afb1175178ced33012d558a817be54851
```

The implementation archive was `<saver-source-archive>`.
Keep the original archive unchanged. Supply its path explicitly; the tests never
download and fail with a prerequisite error if it is absent. The test extractor
verifies the hash before extraction and rejects absolute paths, parent traversal,
links and non-file/non-directory members.

After verifying and safely extracting that archive into a fresh scratch directory,
apply from its `torch_memory_saver-0.0.9.post1` root:

```bash
patch --batch --fuzz=0 -p1 -i /absolute/path/to/runtime/patches/torch-memory-saver-0.0.9.post1-observer.patch
```

The patch adds `csrc/snapshot.h` and changes only `core.h`, `core.cpp`, and the
shared `entrypoint.cpp`. Both existing Torch and preload build variants therefore
export `tms_snapshot_v1`. Existing allocation selection, pause/resume selection,
physical calls, return values and fatal checked-driver behavior stay in place.
Additional bookkeeping invalidates observation after unexpected exceptions,
counter overflow, duplicate metadata insertion or a failed fallback free.

## CPU verification

From the repository root, with Python 3.9+ and a C++17 `g++` compiler:

```bash
PYTHONDONTWRITEBYTECODE=1 TMS_SOURCE_ARCHIVE=<saver-source-archive> python3 -m unittest discover -s runtime/tests -p test_memory_saver_observer.py -v
git diff --check -- runtime/
```

The tests apply the patch to a fresh temporary tree and compile the actual
`core.cpp` and `entrypoint.cpp` with `-std=c++17 -pthread -DUSE_CUDA`. All driver
symbols come from CPU stubs. No CUDA/Torch/engine package is imported, linked or
called. Compile diagnostics fail the suite (`-Wall -Wextra -Werror`, excluding
upstream unused helper/parameter warnings). Two shared libraries exercise both
hook defines. Test-only `-Bsymbolic` keeps their singleton state separate.
Condition-variable gates test mapping-before-insertion and erasure-before-unmap;
there are no sleep-based race tests. Child processes isolate fatal upstream exits.
Test-only private access exercises unreachable counter overflow and malformed
metadata without adding production test methods. Build trees are removed after
the test class; no fixture library belongs in an engine environment.

## Observation contract

The C ABI takes exactly version 1 and a 104-byte record, with checked field offsets.
Capacity is 1–4096; failure clears a valid count pointer and does not write records.
The caller must supply writable, correctly sized buffers. Metadata locking uses
try-lock; concurrent mutations return Busy. Every successful observation contains
the complete map across all tags. ROCm returns Unsupported; this module does not
compile or qualify ROCm. No driver call or allocation policy decision occurs in
the observer export.

`observe_saver(cdll, require_no_backup=True)` in `runtime/memory_saver_observer.py`
uses only the supplied library and makes one bounded export call, without retries.
Pass the initialized saver implementation's existing `_binary_wrapper.cdll`.
At this pin, the package singleton contains it at
`_memory_saver._impl._binary_wrapper.cdll`; `_impl is None` must fail, never call
the lazy initializer. `TorchMemorySaver.enabled` alone is not initialization proof.
Obtaining that instance must not initialize a saver or a CUDA pool during observation. Missing
symbols fail; the reader never constructs a replacement `ctypes.CDLL`.

`runtime/sglang_saver_binding.py` now binds an existing pinned scheduler to this
reader. It checks the exact loaded singleton/hook/pool chain, enrolled process,
protected library hash, and export mappings before and after one snapshot. The
CPU fixtures do not establish CUDA allocator interposition. An authenticated
in-scheduler observation hook and reviewed native build remain required.

Returned frozen aggregates group by device and UTF-8 tag. They include allocation
and state counts, virtual bytes, mapped bytes, backup bytes and the count of
allocations with backup enabled. Addresses remain private to validation. Unknown
tags are preserved for downstream recipe validation. Backup may be enabled before
any backup pointer exists, and a resumed allocation may retain its backup; both
states are reported by the generic reader. The default recipe check rejects any
enabled or present backup, matching the selected SGLang recipe.

ACTIVE contributes its virtual size to mapped bytes. PAUSED contributes zero
mapped bytes while retaining virtual size. These are scoped facts inferred from
the pinned successful driver transitions, not independent hardware queries.
An empty observation does not prove saver enablement or model release. Snapshot
success is not a lifecycle acknowledgement, reservation release, qualification,
complete process footprint or permission to dispatch. On Spark, saver, Torch and
logical tensor observations can overlap; physical capacity must be counted once.

## Required downstream provenance and qualification

Before any isolated CUDA build or installation, integrate this observer into the
protected scheduler, review the integration and build configuration, and retain:

- Source release/archive hash and this patch's SHA-256 from `sha256sum`.
- Exact build commands, compiler, architecture, CUDA/Torch/engine versions and
  selected hook variant, plus hashes of the built wheel and shared library.
- The actual loaded library's resolved path, binary hash/build identity, matching
  saver instance and owning process identity; an on-disk candidate is not proof
  that the running saver uses it.
- The selected recipe, effective backup setting, device/tag scope, and live
  allocation/pause/resume and failure evidence on the separately approved host.

Installed provenance, native lifecycle acknowledgement, whole-process attribution,
CUDA correctness and production composition remain downstream work. Keep
conservative reservations and unknown whole-process residency until those gates
provide evidence. This module performs no install, launch, network access or Spark
operation and does not qualify F2B or F2.

## Upstream license

The original archive's `LICENSE` is retained unchanged when applying the patch.
Upstream notice and license, reproduced here for the included patch context:

```text
MIT License

Copyright (c) 2024 fzyzcjy

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
