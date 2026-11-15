# Status: GPU memory and host RAM shown apart — 2026-11-14 (branch `fix/memory-split`)

SPEC §7.2, ADR 0019 (found live 2026-11-14): `capyctl list hosts` and the web Overview's status line added every memory domain together, so a 16 GB discrete card beside 61 GiB of host RAM read as 77.2 GiB, as if all of it could hold a model. `list hosts` now has a `GPU (FREE / TOTAL)` and a `RAM (FREE / TOTAL)` column, and the status line reads `… GPU free of … · … RAM free of …`. A host with no GPU domain, such as a GB10, has one unified pool, shown under GPU with `unified` in the RAM column, and the web host card now names it "Unified memory (GPU and CPU)".

The same check fixed two web bugs. The server sends only the `device` and `system` domain kinds, but the web UI matched `host` and `unified`. As a result, on a discrete-GPU host the host-RAM bar drew each model's whole footprint instead of its host-RAM share, and on a unified host the card was labelled "Host RAM" and parked memory was not drawn. One rule (`presentation.ts` `memoryPool`) now decides the pool for the status line, the labels and the bar.

Tests, each failing before and passing after: `hosts_show_gpu_memory_and_host_ram_separately` (CLI, T07); the web watch tests for the split status line and the unified pool; the Overview segment test, whose fixture now uses the server's `system` kind. CPU and Fake-engine tests are not qualification; the live check is `capyctl list hosts` and the Overview on a discrete-GPU host and on a GB10.

# Release note: Fixes

- **GPU memory and host RAM are shown apart.** `capyctl list hosts` has separate `GPU` and `RAM` columns, and the web UI's Overview reports GPU memory free and host RAM free separately. Before, they were added together, so a 16 GB card beside 61 GiB of RAM showed as 77 GiB. On a unified-memory machine such as a GB10, the single shared pool is shown as GPU memory and labelled unified.
