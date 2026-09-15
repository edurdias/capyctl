---
product_contract_source: legacy-requirements
---

# F2 Native Checkpoint Preflight Implementation Plan

**Goal:** Provide a pinned local Qwen3 checkpoint verifier and revalidation contract for future native launch composition, covering exact artifacts and tensor geometry.

**Architecture:** A standard-library Python verifier streams the pinned artifacts through trusted file descriptors, checks complete safetensors storage and architecture, and returns immutable scoped facts. A revalidation entry repeats verification before the later protected wrapper starts loading. This is a concrete prerequisite consumed by future native launch composition, not a new qualification or dispatch authority.

**Tech Stack:** Python 3.12+, standard library, Linux descriptor-relative filesystem operations, SHA-256, unittest. No Torch, safetensors, engine or tokenizer imports.

**Spec:** [F2 design](../../design/milestones/f2-sglang-design.md), sections 3, 4, 6 and Q11; [native adapter plan](2026-09-12-f2b-sglang-adapter.md), selected checkpoint/recipe. Exact previously verified source facts are in `.superpowers/sdd/2026-09-12-f2a2d-coordinator-integration/task-7-native-local-inputs.md`.

## Global Constraints

- “Security permission and technical qualification are separate checks.”
- “No blind repetition of a possibly applied engine operation is allowed.”
- “Readiness stays closed through allocation restoration, weight restoration, required cache invalidation/reset, and the qualified model-usability check.”
- Verification performs only bounded file reads. It never imports model code, loads tensors, downloads, edits checkpoints, initializes an engine, arms a store step, or grants admission.
- Only host-a is authorized for any subsequent remote verification. Never access host-b, modify existing engine environments/drivers, or reboot.
- Never read, edit, stage, format, compile or test `crates/mllm-cli/tests/live_interactive.rs`.
- One physical memory domain is counted once. Checkpoint payload bytes are not a process-memory estimate, allocation cap, parked residue, or release acknowledgement.

## Scope and fixed artifact contract

Support exactly `Qwen/Qwen3-4B-Instruct-2507` revision `cdbee75f17c01a7cc42f958dc650907174af0554`. Public entrypoints select this compiled-in manifest; callers cannot supply alternate expected hashes, geometry, revision or test mode. Other models remain unsupported rather than silently approximated. Internally parameterized helpers may enable small fixtures; they are not exposed by a command/API or used to accept runtime configuration.

Create `runtime/checkpoint_preflight.py`, `runtime/checkpoint_manifest.py`, `runtime/tests/test_checkpoint_preflight.py`, and `runtime/README.md`. Existing observer files remain unchanged. No Rust or production routing changes belong to this prerequisite.

Pinned SHA-256 values:

| File | SHA-256 |
| --- | --- |
| config.json | 5beea1a4a34c62782bfb2f911c606741a3bab8f92d80a118fa053c28af12e8ba |
| model.safetensors.index.json | d6c42883a895dfef5b0080ed2116a1bcd764f558406b98923d675978a1abf29c |
| model-00001-of-00003.safetensors | 75311d91bb08cf0b882913da464a1e722a31fb44db35208663487efb7a3d8ed6 |
| model-00002-of-00003.safetensors | 0b48adbb1f60e901153d91907ba11ce63bd4b8b584482e730f48808d055dfba1 |
| model-00003-of-00003.safetensors | 7dd39ccca5e4de123c74c14af44c9bf2eb75df33b4614382af0134528e060d5d |
| tokenizer_config.json | a62ff0a2472a0fa1b8eaabcb57c59b58afa42a22831dc141400b6e0cf2b65ce3 |
| tokenizer.json | aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4 |
| vocab.json | ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910 |
| merges.txt | 599bab54075088774b1733fde865d5bd747cbcc7a547c5bc12610e874e26f5e3 |
| generation_config.json | 835fffe355c9438e7a25be099b3fccaa98350b83451f9fd2d99512e74f1ade48 |

Shard sizes including headers are exactly 3,957,900,840; 3,987,450,520; and 99,630,640 bytes. Tokenizer/config/index artifacts are at most 16 MiB each; safetensors header length at most 1 MiB per shard. Read payloads in chunks at most 1 MiB; never read a shard into memory. Total reads are bounded by the manifest sizes and ancillary limits. Hash every byte, including tokenizer and generation files, using the same descriptor whose identity and parsing were checked. Require ordinary regular files, no symlinks or special files. Extra directory files are not loaded or attested; returned inventory names only the ten required files. No arbitrary directory recursion.

The root must be an absolute normalized path with no `.`/`..` components. Resolve from `/` component by component using `O_DIRECTORY|O_NOFOLLOW` and descriptor-relative opens. Retain root and ancestor descriptor identities throughout the operation; re-resolve the absolute chain and required file entries before returning to detect parent/root/leaf replacement. Open files with no-follow and nonblocking flags so FIFOs cannot hang; then require regular type, expected size/bounds and identity. Check `(device,inode,size,mtime_ns,ctime_ns)` before and after reading each file, and recheck all entries at the end. Reject a changed/missing entry, path escape, symlink, special file, overflow or I/O failure with a sanitized category. Close all descriptors on success and failure.

This detects replacement and ordinary concurrent modification during verification, not immutable storage or containment against malicious same-user writers. A returned result is a moment-in-time observation. Later native composition must revalidate immediately before loading and retain its trusted runtime/filesystem assumptions; this module does not claim to close the race between final verification and an engine reopening a pathname.

## Geometry and storage contract

JSON parsing rejects duplicate keys, invalid UTF-8, NaN/infinity, excessive nesting (maximum 32), booleans where integers are required, and integers outside unsigned 64-bit bounds for sizes/offsets/shapes. Enforce bounded containers before interpreting them: at most 4096 tensor/index entries, tensor names at most 256 UTF-8 bytes, at most eight shape dimensions. Accept only the pinned format's known structures; reject unknown tensor record fields except safetensors top-level `__metadata__` string pairs. Validate index top-level `metadata` and `weight_map`; index `metadata.total_size` is advisory, not sizing authority.

Require Qwen3ForCausalLM, BF16, 36 layers, hidden size 2560, intermediate size 9728, 32 attention heads, 8 KV heads, head dimension 128, vocabulary 151936, tied embeddings, no enabled sliding window, no rope scaling, quantization or hybrid/MoE declaration. Check config values explicitly against this recipe. Model maximum context is 262144; report it as model metadata, never as the deployment's selected 4096 context.

Generate the exact expected tensor-name/shape map from validated geometry, not a count-only test. It comprises embedding `[151936,2560]`, final norm `[2560]`, and these eleven tensors for every layer 0..35:

| Suffix after `model.layers.N.` | Shape |
| --- | --- |
| input_layernorm.weight / post_attention_layernorm.weight | [2560] each |
| mlp.down_proj.weight | [2560,9728] |
| mlp.gate_proj.weight / mlp.up_proj.weight | [9728,2560] each |
| self_attn.k_norm.weight / self_attn.q_norm.weight | [128] each |
| self_attn.k_proj.weight / self_attn.v_proj.weight | [1024,2560] each |
| self_attn.q_proj.weight | [4096,2560] |
| self_attn.o_proj.weight | [2560,4096] |

Every tensor must be BF16, have the exact shape and two ordered nonnegative offsets, and occupy exactly checked-product(shape)*2 bytes. Offsets are relative to the payload after the 8-byte little-endian header length plus header. Require complete gap-free, non-overlapping payload coverage, no trailing bytes, no duplicate tensor across shards, and exact equality of header names, expected names and index assignments. File sizes and payload/header boundaries must agree. No separate `lm_head.weight` is accepted. Expected total: 398 tensors and 8,044,936,192 payload bytes.

The pinned index reports 8,045,591,552 bytes, exceeding payload by 655,360. Preserve its hash and report both values plus this discrepancy; do not reject the exact published artifact solely for that advisory mismatch or change files. Reject any geometry/storage/hash mismatch. Existing full-file hashes and source investigation establish this selected discrepancy, not a generic tolerance for corruption.

## Public interface and errors

```python
def verify_checkpoint(root: str) -> VerifiedCheckpoint:
    """Verify the compiled-in pinned checkpoint, with no engine effects."""

def revalidate_checkpoint(previous: VerifiedCheckpoint) -> VerifiedCheckpoint:
    """Repeat full verification and require the same root/file identities."""
```

`VerifiedCheckpoint` is a frozen dataclass containing model repository/revision, immutable verified artifact records, root/ancestor identities, tensor count, payload bytes, model maximum context, advisory index total and discrepancy. Store the root path privately with `repr=False` for revalidation; never include raw parsed JSON or file contents. Artifact records contain names, verified SHA-256, sizes and stat identities. Equality with reconstructed caller data grants no authority; no serialization/management endpoint is added. Revalidation recomputes hashes and geometry, rejecting even a byte-identical inode replacement. Neither entrypoint skips verification based on cached stats.

Raise `CheckpointPreflightError` with a closed `code` from `invalid_root`, `unsupported_platform`, `unsafe_file`, `artifact_missing`, `artifact_changed`, `artifact_mismatch`, `invalid_json`, `invalid_storage`, `unsupported_geometry`, `io_error`. Messages and repr must omit root paths, raw metadata, file contents and exception details; use `raise ... from None` at public boundaries. No automatic retries. Unsupported operating systems fail explicitly; no weakened path-check fallback.

## Checkpoint 1: Descriptor-safe pinned artifact verification

- [ ] Add small temporary fixture trees and tests before implementation. Exercise the public manifest selection and private small-fixture helper independently; tests cannot make public production pin acceptance configurable.
- [ ] Obtain behavioral RED for missing/mismatched artifacts and replacement rejection, then implement bounded reads, exact hashes and identity checks.
- [ ] Test all ten manifest identities, file size and byte changes, absent files, root and intermediate symlinks, leaf symlinks, FIFO/directory leaves, parent/root/file replacement, in-place mutation, descriptor cleanup, read limits and sanitized errors. Use explicit barriers/hooks around private read stages for mutation tests, never timing sleeps.

```python
def test_public_verifier_does_not_accept_fixture_hashes(self):
    with self.assertRaises(CheckpointPreflightError) as caught:
        verify_checkpoint(str(self.small_fixture_root))
    self.assertEqual(caught.exception.code, "artifact_mismatch")
```

## Checkpoint 2: Complete native geometry and storage verification

- [ ] Write geometry/storage regressions against small valid fixture geometry through private helpers. Separately assert the actual selected geometry generates all 398 exact names/shapes and 8,044,936,192 bytes, without allocating those payloads.
- [ ] Implement bounded strict JSON and safetensors header parsing using the same verified descriptors. Test duplicate JSON keys, oversized/deep headers, bad types/offsets/products, unknown tensor fields, wrong dtype/shape/layer, missing/extra/duplicate tensors, wrong shard assignment, gaps/overlaps/trailing bytes and tied-embedding rules.
- [ ] Preserve the known advisory aggregate discrepancy in a positive fixture and reject semantic/hash corruption. A small fixture's hashes may be generated internally; label it synthetic, never native artifact proof.

```python
def test_selected_geometry_exact_payload(self):
    expected = _expected_tensors(_PINNED_GEOMETRY)
    self.assertEqual(len(expected), 398)
    self.assertEqual(sum(_tensor_bytes(shape) for shape in expected.values()), 8044936192)
```

## Checkpoint 3: Revalidation, integration contract and module gate

- [ ] Obtain revalidation RED for unchanged-byte inode replacement and changed contents; implement complete reread/hash/geometry verification with exact identity comparison. Verify immutable returned tuples/records, no root path or metadata leakage, and no model/engine imports or subprocess/network effects.
- [ ] Document public entrypoints, exact pinned scope, normal and failure usage, bounded I/O, filesystem-cache warming, synthetic fixture limits and the downstream pre-arm/pre-effect consumer contract in `runtime/README.md`. Existing isolated environment remains unchanged.
- [ ] Run focused tests to GREEN, then all runtime tests once, inspect nonzero discovered counts, self-review and freeze for the sole independent module review. Root separately runs canonical Rust/Clippy gates before local merge. No checkpoint commit or per-checkpoint reviewer.

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s runtime/tests -p test_checkpoint_preflight.py -v
PYTHONDONTWRITEBYTECODE=1 TMS_SOURCE_ARCHIVE=<saver-source-archive> python3 -m unittest discover -s runtime/tests -p 'test_*.py' -v
git diff --check
```

After independent review, root may run this exact verifier read-only against the already present checkpoint on host-a without installing or importing an engine. Such a result qualifies only checkpoint files at observation time, not the engine, phase budgets, runtime provenance or F2. If unavailable, report that limitation; do not download, replace files or weaken validation.

## Completion boundary

Complete when pinned artifact verification, geometry/storage checks, revalidation and documented consumption contract pass deterministic tests and one independent module review. Native descriptor normalization, trusted installed-engine provenance, protected launch/IPC/enrollment, real allocation evidence, coordinator consumption, production cutover and live F2 qualification remain separate required work. This module does not label the full native execution path complete.
