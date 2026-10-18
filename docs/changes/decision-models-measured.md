# Status: Decision models measured on one GB10; the guide's llama.cpp example — 2026-10-18 (branch `docs/decision-models-guide`)

Live on host A, 2026-10-17 to 2026-10-18, `main` at `075023d7` and `8824b622`, isolated
standalone, managed limit 108 GiB: eight decision deployments (Clef, Clef-Flash, Kev-4B,
lev, Julia-1 and Laya on llama.cpp v0.7.0; Clef and Clef-Flash on SGLang 0.5.22) became
ready, answered `POST /v1/systemone` through the router, and ran capyctl-bench 0.3.0's
decisions suites (about 4,200 requests each: latency grid, cache, throughput at 1 to 8
streams, AG News, BoolQ, Typed Decisions, and the Decision Index components API-Bank,
GPQA Diamond and Home appliances). Published scores reproduce: Clef 91.9% on API-Bank,
Clef-Flash 93.1% (SGLang), Julia-1 73.0% on Typed Decisions. The results, six recipes and
the comparison are in capyctl-recipes (`comparisons/decision-models-gb10`).

Findings for CapyCTL:

- llama.cpp v0.7.0 reads a decision request's whole input in one batch; with its default
  batch sizes a longer input is refused (HTTP 500, "increase the physical batch size").
  Every measured llama.cpp deployment sets `--batch-size` and `--ubatch-size` to the
  context. The guide's example (`docs/guide/deploy.md`) now does too and states
  `resources`.
- Decisions/s does not rise with concurrent requests on llama.cpp (flat from 1 to 8) and
  rises 7% to 18% on SGLang. A repeated input is at most 5% faster on either engine.
- CapyCTL's endpoint adds 12 to 26 ms per decision request against llama-server launched
  directly with the same arguments (Julia-1: 19.1 against 7.5 ms at one question about a
  200-token input); at 8 concurrent requests the throughput is the same.
- Measured peaks (`startup.measured`) of the recipes: Julia-1 1.4 GiB, Clef-Flash 15.8 GiB,
  Kev-4B 25.9 GiB and Clef 35.4 GiB on llama.cpp (16,384-token context and batch; 8,192 for
  Julia-1); Clef-Flash 51.9 GiB and Clef 87.5 GiB on SGLang, whose pool follows the
  reservation.

Documentation only; no code changed. These are live measurements of the listed engines and
checkpoints, not qualification of other recipes.

# Release note: Decision models

- **Measured recipes.** CapyCTL's recipes repository has decision-model recipes for
  Clef-Flash, Clef, Kev-4B and Julia-1 on llama.cpp v0.7.0 and Clef-Flash and Clef on
  SGLang 0.5.22, and a comparison of six decision models on one GB10: speed, throughput
  and quality on five public test sets. On llama.cpp, set `--batch-size` and
  `--ubatch-size` to the context; the [guide](../guide/deploy.md#decision-models) shows
  how.
