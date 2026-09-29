# Security policy

mllm is early software. Security fixes target the latest release; older releases
have no separate maintenance commitment. Run current versions of mllm and the
inference engines you use, and review their release notes before upgrading.

## Report a vulnerability privately

Use GitHub's **Report a vulnerability** action on the repository's Security tab:
[private vulnerability report](https://github.com/edurdias/mllm/security/advisories/new).
This channel becomes available after the repository is public and maintainers
have enabled private vulnerability reporting. If the action is unavailable, do
not put exploit details or secrets in a public issue. You may open an issue asking
maintainers to enable private reporting, without disclosing the vulnerability.

Include affected versions, impact, minimal reproduction steps and suggested
mitigations if known. Redact keys, enrollment material, private prompts/model data
and identifying host details. Allow maintainers to investigate and coordinate a
fix before public disclosure. No response deadline or bounty is promised.

## Deployment boundaries

Engine control listeners are restricted to loopback, protected by per-launch
keys and separate from the public inference endpoint. Parking uses engine
development controls that are not production-hardened. Keep those protections
in place; a host can opt out of deep parking with `--deep-park off`.

The inference endpoint requires an API key. Default HTTP does not encrypt traffic;
use an appropriately secured network or TLS proxy when accessing it remotely.
See [network access](docs/operations/network-access.md) and the
[configuration reference](docs/operations/configuration.md). Do not expose engine
control ports as a workaround for connectivity problems.

mllm does not install or maintain your engines, GPU drivers or model weights.
Report vulnerabilities in those components to their maintainers; report problems
in mllm's integration here. Passing CPU or Fake-engine tests is not native engine
qualification or a security audit.
