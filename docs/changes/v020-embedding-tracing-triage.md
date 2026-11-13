# Status: Correct embedding and tracing qualification fixtures — 2026-11-12 (branch `live/v020-triage-em`)

ADR 0036 EM5 now uses the supported 30-second request deadline and allows it to expire while retaining the engine connection. EM7 reads the persisted policy's controls and gpu0 domain mapping, including the unified system pool. EM9 waits up to 60 seconds for the latest completed park's same-launch residue: lifecycle completion precedes the asynchronous host measurement. The committed release, latest-step identity, measured residue and retained launch requirements remain mandatory.

EM8's budget increases from 30 minutes to four hours without removing any cell. Host A's vLLM cells alone consumed 1567 seconds; the largest cell had roughly 303-second median latency. Only 29 seconds of completed SGLang cells fit before the original overall deadline expired. This evidence supports an undersized qualification budget, not a demonstrated SGLang runtime stall. A complete rerun must still finish every engine's full matrix, retrieval check and large-answer memory checks.

ADR 0037 OT7 propagates a skipped OT8 baseline as unavailable instead of reporting a product failure; failed or missing baselines still fail. Tagged CPU regression tests cover the deadline, both policy topologies, delayed/stale residue samples and skipped-versus-failed tracing prerequisites. CPU tests are not qualification. All five rows still require live reruns; existing engine environments and server ledgers were not modified.

# Release note: none
