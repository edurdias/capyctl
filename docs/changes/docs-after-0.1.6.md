# Status: Summaries brought up to 0.1.6 — 2026-10-21 (branch `docs/after-0.1.6`)

Docs and website copy only. The README's list of what works in 0.1.6 gains
custom engines, request IDs and decision metrics, engine logs kept in memory,
the controller guide and running a role in a container, each linked to its
guide page; its documentation list links the user guide, the container section
and the 0.1.6 release notes. `docs/README.md` links the user guide and the
0.1.5 release notes, which it had skipped. The guide's overview, install,
how-it-works and engine options pages mention custom engines, and the install
page gains "In a container", which points to the operations guide's container
section and states what has not run in a container yet. The operations guide's
engine log section and the data retention page no longer say that no engine
logs prompts: they name llama.cpp, and say that a custom engine logs whatever
its manifest's command prints. On the website, the landing page's capabilities
table lists custom engines, containers, decision models and request IDs,
engine logs kept in memory and the controller guide, and the hero line and the
site description name custom engines. No code or tests change; CPU and
Fake-engine tests are not qualification, and no live check is needed.

# Release note: none
