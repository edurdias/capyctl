//! The build this binary is: its version and, for a release build, the commit
//! it was built from.
//!
//! `packaging/release.sh` sets `CAPYCTL_BUILD_COMMIT` for the compile to the
//! commit the tarball's BUILDINFO records, with `-dirty` appended when the
//! tree had uncommitted changes (`CAPYCTL_RELEASE_ALLOW_DIRTY=1`). A plain
//! `cargo build` records none. The value is fixed for a given commit, so a
//! rebuild of it stays byte-identical.
//!
//! `capyctl --version` stays exactly `capyctl <version>`: the installer and
//! the packaging check compare that whole output, and an installer from an
//! earlier release does the same against a newer binary. `capyctl version`
//! prints the same line first and the commit after it (`views`).

use serde_json::{json, Value};

/// The result of `capyctl version`: `commit` is null when the build recorded
/// none.
pub fn json() -> Value {
    let commit = option_env!("CAPYCTL_BUILD_COMMIT").filter(|commit| !commit.is_empty());
    json!({"name": "capyctl", "version": env!("CARGO_PKG_VERSION"), "commit": commit})
}
