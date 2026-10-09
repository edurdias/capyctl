# Status: Faster checkpoint readiness — 2026-10-08 (branch `feat/digest-readiness`)

Owner decision 2026-10-08 (ADR 0014 §7, amendment of 2026-10-08): option (a) now, option (b) only with host approval. (a) `sources.rs` computes each file's SHA-256 while it streams (beside the git blob id where that is the pin), notes it with the file's stat identity right after the rename, and writes an owner-only `<id>.manifest` at commit; `CheckpointVerifier` adopts it when the walked files and identities match and the small files rehash to it (`fetched`, no weight file read; same canonical manifest, so the same digest). Tar extractions and unnoted files are measured. (b) `checkpoints.trust_declared_digest` (flag `--trust-declared-digest`, `CAPYCTL_TRUST_DECLARED_DIGEST`, YAML; default off) lets a host take a local source's canonical declared digest on first sight, hashing only small files and seeding the stat cache; a changed file forces a full rehash, the policy off forces a measurement, and the store records `declared_trusted` only for the revision's own declaration. Provenance travels as `CheckpointDigestEvidence.provenance` (field 10, additive), is stored in `checkpoint_digests.provenance` (v46; earlier digests `measured`) and shown by status.

Tests (T03 T14 T34): `a_fetched_checkpoint_digest_needs_no_second_read` (reads counted: only small files), `a_changed_or_unpinned_download_is_measured`, `a_declared_digest_is_trusted_only_when_the_host_allows_it`, `a_trusted_declaration_still_catches_a_changed_file`, `digest_checkpoint_trusts_a_declaration_only_by_host_policy`, `the_embedded_host_trusts_a_declared_digest_only_by_policy`, `a_recorded_digest_keeps_where_it_came_from`, `a_recorded_digest_keeps_its_provenance`, `v46_marks_earlier_digests_measured_idempotently`, `declared_digest_trust_follows_flag_env_document_default`, `status_shows_how_a_digest_was_recorded`, protocol validation. CPU tests only; not qualification. A live Hugging Face download and first start on a lab host is pending. Schema v46 and the dated ADR amendment may collide with other open branches that also add v46; renumber on merge.

# Release note: Checkpoints

- **A downloaded model is ready sooner.** A Hugging Face or `http` model CapyCTL
  downloads is checked file by file against its pins as it arrives. Its
  checkpoint digest is now built from those checks, so the first start no
  longer reads every weight file a second time. The digest is the same one a
  full read gives. A file changed after the download, and a tar archive's
  contents, are still read in full.
- **A host may trust a declared digest.** With
  `checkpoints.trust_declared_digest: true` (`--trust-declared-digest true`,
  `CAPYCTL_TRUST_DECLARED_DIGEST=true`; off by default), a host takes a local
  model's declared `model.content_fingerprint` as its digest the first time it
  sees the model, reading only the small files. Any later change to a file is
  measured in full, and a digest that no longer matches is refused. Turning the
  setting off measures the model again.
- **Status says where a digest came from.** `capyctl status deployment`
  reports `provenance` (`measured`, `fetched` or `declared_trusted`) with the
  checkpoint digest, and says so in text when the digest was not measured in full.
