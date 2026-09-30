# Questions and feedback

Start with the [README](README.md), [installation guide](docs/guide/install.md)
and [configuration reference](docs/operations/configuration.md).

Use [GitHub Issues](https://github.com/edurdias/capyctl/issues) for public questions,
bug reports, feature requests and documentation feedback. Search existing issues
first, then choose the closest template. For a usage question, choose the
question template in the issue chooser. Include what you are trying to do, what you
have tried and the relevant version and configuration.

For a useful bug report, include:

- `capyctl --version`, Linux distribution and architecture.
- GPU model and memory, driver version, and engine name and exact version.
- Standalone or server/host setup, model identifier and relevant deployment fields.
- Minimal steps, expected behavior, actual behavior and relevant redacted errors.
- Whether the result came from a native engine, a CPU test or a Fake engine.

Share only the smallest evidence needed. Remove API keys, enrollment files,
certificates and private keys, private prompts/model data, home paths, hostnames
and addresses you do not want public. Do not attach a complete state directory,
SQLite ledger or unreviewed diagnostic archive. Use generic names consistently
so the report remains understandable.

For feature requests, describe the use case and what you do today. For performance
feedback, include the workload, hardware, engine/model versions, configuration,
measurement method and repeat results; a single latency number is not enough to
compare setups.

This is a community project with no guaranteed response time or support contract.
Suspected vulnerabilities belong in [SECURITY.md](SECURITY.md), not public issues.
