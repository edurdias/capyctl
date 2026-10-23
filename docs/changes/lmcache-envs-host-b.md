# Status: LMCache 0.5.6 environments on host B — 2026-10-22 (branch `docs/lmcache-envs-host-b`)

Owner approval 2026-10-22 (ADR 0033): host B gets byte-identical copies of host A's two
LMCache environments, so the ADR 0033 LC rows can run on either host. No code changes;
there are no tests. CPU and Fake-engine tests are not qualification.

**Copies.** `rsync -a` from host A to host B over the direct link, same paths:

- `~/vllm-0.32.0-lmcache-venv`: 70,203 files, 4 symlinks, 8,250 directories.
- `~/sglang-0.5.22-lmcache-venv`: 89,994 files, 4 symlinks, 10,708 directories.

For each environment, a manifest was computed on both hosts: the SHA-256 of every
regular file, every symlink with its target, and every directory with its mode. Each
manifest is identical on the two hosts (vLLM `adbeb70b7f0a…`, SGLang `ac50e7d84930…`).
`rsync -a` does not keep hard links. On host A the SGLang environment shares hard-linked
files with the vLLM one, so on host B it takes 9.0 GiB of disk instead of 4.9 GiB; the
contents are the same.

**Models.** Host B's `~/models/qwen3-4b-instruct` (Qwen3-4B-Instruct-2507) is the same
checkpoint as host A's: its 13 checkpoint files have the same SHA-256 on both hosts. Only
the `.cache/huggingface/` download metadata differs, and CapyCTL leaves that out of the
checkpoint digest. Clef-Flash was not in host B's `~/models`, so host A's
`~/models/clef-flash` was copied there. All 54 of its files, `.cache` included, have the
same SHA-256 on both hosts.

**Still to check on host B.** Host B was in use by other work, so nothing was run there:
`import vllm, lmcache` and `import sglang, lmcache`, CUDA being available in each
environment, and the LC rows. These wait until the owner says host B is free.

# Release note: none
