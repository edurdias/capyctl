# Contributing to CapyCTL

Bug reports, documentation fixes, tests and focused code changes are welcome.
For questions and feedback, start with [SUPPORT.md](SUPPORT.md). Report suspected
vulnerabilities through [SECURITY.md](SECURITY.md), not a public issue.
Participants follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Before changing code

Read [AGENTS.md](AGENTS.md), the working agreement for contributors. It points to
the authoritative specification, design decisions and current status. Open an
issue before a substantial feature or architecture change so maintainers can
confirm its scope. Small fixes can go straight to a pull request.

## Development setup

Use Linux with Git, the stable Rust toolchain, `rustfmt`, `clippy`, a C/C++ build
toolchain, `pkg-config`, OpenSSL development headers and Protocol Buffers compiler
(`protoc`) on `PATH`. The repository's
`rust-toolchain.toml` selects stable Rust; `Cargo.lock` fixes dependency versions.
For example, Debian/Ubuntu supplies the native tools through `build-essential`,
`protobuf-compiler`, `pkg-config` and `libssl-dev`. Install Rust separately using your usual toolchain
manager. The CPU tests do not need a GPU or an installed inference engine.

Fork the repository, clone your fork and create a branch from `main`. From the
repository root:

```bash
rustup component add rustfmt clippy
cargo build --locked -p capyctl-cli
cargo run --locked -p capyctl-cli -- --help
```

See the [installation guide](docs/operations/install.md) for running CapyCTL with
your own engine. Development checks must not change drivers or shared engine
environments.

## Verification

Run these checks from the repository root before submitting code:

```bash
cargo fmt --all --check
cargo test -p capyctl-adapters -p capyctl-store -p capyctl-controller -p capyctl-management \
  -p harness --all-targets --no-fail-fast --locked -- --test-threads=4
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The core suite above is the authoritative integration run. One owned-state test
spawns a child whose nested summary duplicates one reported test; count that test
once when reporting totals. Include the commands and results in your pull request,
including any failures or checks you could not run. Automated checks supplement
this evidence; they do not replace it.

Python runtime changes also need Python 3.11+, `g++` with C++17 support and `patch`.
The full runtime suite requires the exact legacy saver source archive described
in [runtime/patches/README.md](runtime/patches/README.md); tests verify its hash
and do not download it. Fetch and verify the fixture without installing an engine:

```bash
export TMS_SOURCE_ARCHIVE=/tmp/torch_memory_saver-0.0.9.post1.tar.gz
curl -fL --retry 3 --output "$TMS_SOURCE_ARCHIVE" \
  https://files.pythonhosted.org/packages/81/fd/42aad783d433fd69dc108b1b2ee5860fcf33e20e5440b899bc004ff97d70/torch_memory_saver-0.0.9.post1.tar.gz
printf '%s  %s\n' \
  25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43 \
  "$TMS_SOURCE_ARCHIVE" | sha256sum -c -
```

After the checksum passes, run:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s runtime/tests -p 'test_*.py' -v
```

A missing fixture is an unmet prerequisite, not a passing full runtime suite.

For installer changes run `scripts/test-install.sh`. For packaging changes run
`CAPYCTL_VERIFY_STRICT=1 scripts/verify-packaging.sh`; this additionally needs
ShellCheck, `systemd-analyze` and the tools used by
[the release script](packaging/release.sh). For website changes use Node as
specified in `site/.nvmrc`, then run:

```bash
cargo fetch --locked
cd site
npm ci
npm run check
```

**CPU and Fake-engine tests are not native engine qualification.** Engine claims
need a reproducible native run with the exact engine version, model, hardware,
configuration and lifecycle results. See the
[live harness guide](scripts/live/matrix/README.md). Run live tests only on
hardware you are authorized to use; preserve ownership and memory accounting
when a result is uncertain.

## Pull requests

Keep each pull request focused on one problem and target `main`. Explain the
before/after behavior, relevant issue or requirement, and how you checked it.
Add a regression test when a behavior change needs one; documentation-only edits
do not need artificial tests. Cite the governing specification requirement near
spec-driven code and tag acceptance tests with their T01–T40 matrix ID.

Update user documentation when behavior changes. Implementation status lives in
`docs/runbooks/f2-current-status.md`, but a pull request does not edit it or the
release notes: it adds one change file, `docs/changes/<short-slug>.md`, holding
its status entry and its release note ([format](docs/changes/README.md)), and
the release folds those files in. Do not add another status report or commit
regenerable logs, build outputs or diffs. Remove credentials, private model data,
prompts, local machine identifiers and personal paths from examples and evidence.

Maintainers review and merge changes. Release publication is an owner action.
By submitting a contribution, you agree to license it under the project's
[Apache-2.0 license](LICENSE). Only contribute material you have the right to share,
and retain applicable third-party notices.
