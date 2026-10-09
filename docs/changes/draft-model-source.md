# Status: Draft model source — 2026-10-08 (branch `feat/draft-model-source`)

A deployment may declare `model.draft`, its speculative drafter's own model source (ADR 0008 amendment 2026-10-08): validated and policy-checked like `model.source`, part of the revision fingerprints (absent: unchanged), materialized and verified as a second source row per host (store schema 47, `model_sources` keyed by source), and waited for before activation, digest and launch. Its weights are sized with the checkpoint's without approvals, its `config.json` feeds the draft KV fit, and CapyCTL renders it to SGLang (`speculative_draft_model_path` via the entry), vLLM (`--speculative-config` `model`, merged with the operator's) and TensorFold (`--drafter`). CPU and Fake tests only; a native SGLang or vLLM launch with a materialized drafter on a lab host is still needed.

# Release note: Checkpoints

- **A drafter can be downloaded beside the model.** A deployment may declare
  its speculative drafter as a source of its own, `model.draft`, in any form
  `model.source` takes (a local path, a pinned Hugging Face commit, an HTTPS
  payload pinned by SHA-256). CapyCTL downloads and verifies a remote drafter
  into the sources store like the weights, waits for both before the
  deployment activates, counts its weights with the model's, and passes its
  directory to the engine (SGLang's draft model path, vLLM's
  `--speculative-config` `model`, TensorFold's `--drafter`), with no
  `approved_paths` entry. The engine arguments still turn speculation on
  (SGLang `--speculative-algorithm`, vLLM `--speculative-config` with
  `num_speculative_tokens`) and may not name a draft path of their own. Hosts
  and the server must run this release; the server's store migrates to
  schema 47. A deployment without `model.draft` is unchanged, fingerprints
  included. See [configuration](../operations/configuration.md#models-and-downloads).
