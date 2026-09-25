# F2B Memory Saver Observer Implementation Plan

**Goal:** Provide a bounded, coherent observation of the actual CUDA memory-saver allocation map, without granting lifecycle authority or changing allocation policy.

**Architecture:** Carry a minimal patch against the exact memory-saver source release. Add one read-only C ABI export and mutation bookkeeping around the existing CUDA allocator. A Python reader consumes the already-loaded saver library, validates the complete snapshot, and returns scoped aggregates without public addresses.

**Tech Stack:** Existing C++17 CUDA source, a CPU driver stub, Python standard library/ctypes/unittest, and the existing runtime directory convention. No new runtime dependency.

**Implementation status:** All three checkpoints completed in `782a3f4`. One consolidated independent review passed specification and quality with no findings. Fresh root verification passed 27 observer tests and 509 existing Rust tests, plus both canonical Clippy checks. This closes only the CPU-tested observer prerequisite; downstream integration, isolated CUDA build, installed provenance and live qualification remain required.

**Spec:** [F2 design](../design/milestones/f2-sglang-design.md), §§3/6 and Q11; [F2B adapter plan](2026-09-12-f2b-sglang-adapter.md), selected recipe and allocator evidence requirements. The detailed source investigation was kept in local implementation notes.

## Global Constraints

- “Parking and restoration must be qualified for at least one selected recipe for each engine to close F2.”
- “Security permission and technical qualification are separate checks.”
- “No blind repetition of a possibly applied engine operation is allowed.”
- This module neither launches an engine nor installs a patch. Only host-a may be used in later separately gated live work. Never access host-b or modify existing engine environments or drivers.
- Preserve `crates/mllm-cli/tests/live_interactive.rs`: never read, edit, stage, format, compile or run it.
- Snapshot success is not a Park/Restore acknowledgement, qualification, complete process footprint, reservation release, or permission to dispatch.
- Read the actual saver instance's `_binary_wrapper.cdll`; never load a second library to obtain a convenient symbol.
- Count Spark physical capacity once. Saver, Torch and logical tensor observations can overlap and must not be added blindly.
- The selected SGLang recipe disables CPU backup. The generic observer reports backup state honestly; its recipe-level aggregation rejects enabled or present backup.

## Scope and fixed decisions

This is one independently testable native prerequisite, not completion of the native execution module or F2B. Protected scheduler integration, native lifecycle acknowledgement, installed artifact provenance, CUDA qualification, whole-process attribution and production composition remain explicit downstream work.

Patch source: `torch_memory_saver-0.0.9.post1`, source archive SHA-256 `25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43`. The previously downloaded archive is `<saver-source-archive>`. Tests must accept an explicit source-archive path, verify its hash before extraction, reject unsafe members, and never download implicitly. A missing archive produces a clear prerequisite failure, not a passing skip.

Do not vendor an entire engine or saver source tree. Commit the minimal unified patch, ABI header, reader, test support, and provenance/application instructions. Tests apply the patch to a fresh temporary source tree and compile the real patched `core.cpp` plus export with driver stubs. They must not test a separately reimplemented allocator model. Preserve upstream license notices and identify the source release.

### Versioned ABI

Add `tms_snapshot_v1` to both existing hook build variants from their shared source. The observation ABI supports CUDA only; a ROCm build returns Unsupported rather than presenting uninstrumented metadata as coherent.

```cpp
enum TmsSnapshotStatus : uint32_t {
    TMS_SNAPSHOT_OK = 0, TMS_SNAPSHOT_BUSY = 1,
    TMS_SNAPSHOT_INVALID = 2, TMS_SNAPSHOT_OVERFLOW = 3,
    TMS_SNAPSHOT_UNSUPPORTED = 4, TMS_SNAPSHOT_INTERNAL = 5
};
struct TmsSnapshotRecordV1 {
    uint64_t address;             // process-private; never public/debug output
    uint64_t size_bytes;
    uint64_t backup_bytes;
    int32_t device;
    uint32_t state;               // 1 ACTIVE, 2 PAUSED
    uint32_t backup_enabled;      // exactly 0 or 1
    uint32_t tag_length;          // 0..63; remaining bytes are zero
    char tag[64];
};
extern "C" uint32_t tms_snapshot_v1(
    uint32_t abi_version, uint32_t record_size,
    TmsSnapshotRecordV1* records, uint32_t capacity, uint32_t* count) noexcept;
```

Version is exactly 1; record size is exactly 104 bytes on the supported 64-bit ABI, with checked field offsets and a matching ctypes layout. Capacity is 1..4096 records. Reject null pointers, unknown versions/sizes and invalid capacities. For any valid count pointer, set count to zero on failure. No exception crosses the C ABI. An empty map is a valid zero-record observation, not proof that a saver is enabled or a model is released.

Use a try-lock on the existing metadata mutex, returning Busy if unavailable. Under the same lock, reject any in-flight mutation, enumerate all tags, validate every record and bound, then copy the complete map. No pagination, silent truncation, driver call, tensor read, log parsing, callback or allocator side effect. Prevalidate before writing records so error results expose no partial snapshot. Sort only in the Python reader if stable comparison needs it; map order is not semantic.

Tags are length-delimited bytes at the ABI; reject embedded NUL or lengths above 63. The reader requires valid UTF-8. Reject negative devices, unknown states, zero or overflowing sizes, invalid addresses and impossible backup state. ACTIVE entries contribute size_bytes to mapped bytes; PAUSED entries contribute zero mapped bytes but retain virtual size. Backup bytes equal the allocation size when a backup pointer exists, otherwise zero. These are scoped facts under the pinned successful driver-transition semantics, not independent hardware queries.

### Coherence boundary

The existing malloc maps before metadata insertion, while free removes metadata before unmapping. A mutex-only reader misses both windows. Add an in-flight counter protected by that same mutex, with an RAII guard beginning before the first CUDA allocation/free effect and ending only after all effects and metadata updates finish. Cover managed malloc, managed free and fallback free paths. Pause/resume already hold the metadata mutex through their checked physical effects; the snapshot returns Busy during those calls. Do not change their policy, error/exit behavior or tag selection.

Guard bookkeeping must not deadlock on returns from locked scopes. Counter overflow or an unexpected exception must never yield an apparently complete snapshot; latch an observation-invalid state if coherent bookkeeping cannot be maintained. Existing fatal CUDA failure still terminates the process; a surviving caller cannot infer success from missing output. Do not add retries, change free-on-paused behavior, or repair unrelated upstream allocation bugs in this patch.

## File map

- `runtime/patches/torch-memory-saver-0.0.9.post1-observer.patch`: minimal upstream changes to core/header/entrypoint and ABI header.
- `runtime/memory_saver_observer.py`: ctypes layout, same-library symbol lookup, strict record validation and bounded per-device/tag aggregation; no engine import.
- `runtime/tests/test_memory_saver_observer.py`: reader and real-patch application/build tests.
- `runtime/tests/memory_saver_stub/`: only driver/API stub headers and implementation needed to compile the patched source, plus deterministic C++ concurrency tests.
- `runtime/patches/README.md`: pinned source hash, patch application, CPU command, limitations and future isolated-build provenance requirements.

## Checkpoint 1: Real-source bounded export

Consumes the pinned source archive; produces the patch and CPU-compilable ABI.

- [ ] Write a test that verifies the source hash, applies the patch in a temporary directory, and compiles the actual patched core/export with `-std=c++17 -pthread -DUSE_CUDA`. A missing patch/export is the expected initial failure.
- [ ] Exercise ABI version/size/null/capacity rejection and an empty actual saver singleton before implementing the export.
- [ ] Add only the versioned ABI, complete-map validation/copy and exception boundary. Use the existing allocation metadata, not a second registry.
- [ ] Add actual stub-backed allocations across multiple devices/tags and assert exact ACTIVE/PAUSED/backup records, all-tag visibility, no driver calls made by snapshot, and no partial record output on bounds failure.

```python
def test_snapshot_does_not_call_driver(self):
    self.driver.allocate(size=4096, device=0, tag=b"weights")
    before = self.driver.call_count()
    result = self.driver.snapshot()
    self.assertEqual(result.mapped_bytes, 4096)
    self.assertEqual(self.driver.call_count(), before)
```

The driver helper in the test support wraps the compiled real allocator and exposes bounded test-only entrypoints; it is not production Python or another allocator implementation.

## Checkpoint 2: Mutation-window coverage

Consumes the same compiled patched source; produces coherent snapshot bookkeeping and deterministic race regressions.

- [ ] Block stub malloc after mapping but before map insertion and assert Busy; block free after map erasure but before unmap and assert Busy. The old mutex-only implementation must fail these assertions.
- [ ] Add the RAII in-flight guard and exception/overflow invalidation. Keep all physical allocation semantics unchanged.
- [ ] Test overlapping malloc/free, pause/resume mutex contention, fallback-free returns, guard cleanup after recoverable C++ exceptions, and stable snapshot after each completed operation. Use barriers/condition variables, never sleep-based race tests.
- [ ] Run checked-driver failure scenarios in child processes, assert nonzero exit and no successful observation emitted after failure. A CPU stub verifies control flow only, not CUDA correctness.

```python
def test_untracked_inflight_mapping_is_busy(self):
    with self.driver.pause_malloc_after_map():
        self.assertEqual(self.driver.snapshot_status(), "busy")
    self.assertEqual(self.driver.snapshot_status(), "ok")
```

## Checkpoint 3: Same-library reader and module verification

Consumes an already-selected `ctypes.CDLL` object and the fixed ABI; produces immutable aggregates, not serializable authority.

```python
def observe_saver(cdll, *, require_no_backup=True):
    """Read tms_snapshot_v1 from this object only; never construct a CDLL."""
```

- [ ] Write reader tests for symbol absence, wrong ABI/layout, each status, malformed records, invalid UTF-8, duplicate/overlapping ranges on the same device, sum overflow, unknown tags, backup enforcement and complete multi-device aggregation.
- [ ] Implement one bounded buffer and one export call with no retry. Preserve all tags in aggregates; downstream recipe validation decides which tags are supported. Keep addresses in temporary private records only, exclude them from repr/errors/returned aggregates.
- [ ] Prove the reader uses only the supplied library by replacing CDLL construction with a failing test double. Test two distinct compiled stub libraries; reading one must never report the other's singleton state.
- [ ] Run all module tests once, verify clean warnings and whitespace, self-review the complete patch and reader, then freeze for one independent module review. No checkpoint commit or reviewer; root commits the complete reviewed module.

Run:

```bash
TMS_SOURCE_ARCHIVE=<saver-source-archive> python3 -m unittest discover -s runtime/tests -p test_memory_saver_observer.py -v
git diff --check
```

## Module completion and next gate

Completion requires genuine RED/GREEN evidence for export and race failures, passing real-patch CPU tests and reader tests, one clean independent module review, exact source/patch identities in documentation and no unrelated runtime changes. A CPU-only compile must never be labelled a deployable CUDA build.

After this module, integrate the observer with the protected SGLang scheduler and record the actual loaded library's provenance. Only then build/install into the approved isolated environment, after reviewing that integration and build configuration. GPU allocation/pause/resume tests and complete native correctness qualification remain required. Keep conservative reservations and unknown whole-process residency until those separate gates provide evidence.
