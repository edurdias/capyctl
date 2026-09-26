//! ADR 0019 (owner decision 1), design §9: the old loopback inference default
//! moves to all interfaces once, on upgrade. The only sanctioned rewrite of an
//! administrator document (SPEC §15.1 as amended): one value, atomically, with
//! the original kept beside it, and never twice.
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The inference bind every generator before design §9 wrote.
pub const OLD_DEFAULT: &str = "127.0.0.1:8443";
/// The inference bind a migrated document states (design §9).
pub const NEW_DEFAULT: &str = crate::standalone::DEFAULT_INFERENCE_BIND;
/// Relative to the role's state directory: present once the migration ran.
pub const MARKER: &str = "migrations/inference-bind-v1";

/// What the first start of this release did to the role document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Migration {
    /// Nothing to do: the migration already ran, or the document does not
    /// state the old default.
    NotNeeded,
    /// The document now states [`NEW_DEFAULT`]; `backup` is the original.
    Rewritten { backup: PathBuf },
    /// The document could not be rewritten safely and is unchanged. The role
    /// binds [`NEW_DEFAULT`] for this run (`config_migration_failed`).
    BindOnly { reason: String },
}

/// Run the one-time migration of `document` for the role whose state root is
/// `state_dir`. `parsed_bind` is the document's inference bind as the caller
/// parsed it (`listeners.inference.bind` for the server,
/// `server.listeners.inference.bind` for standalone).
///
/// Design §9: the marker records that the first start of this release has
/// happened, whatever that start found. A document the operator narrows back
/// to `127.0.0.1:8443` afterwards (the notice tells them how) is then never
/// migrated, and neither is one a fresh installation of this release set to
/// loopback after its first start.
///
/// A start that could not rewrite the document ([`Migration::BindOnly`], for
/// example a packaged server whose `/etc` is read-only) records the migration
/// as pending, not done: the unchanged document still states the old default
/// only because it could not be changed, so every later start binds the new
/// default again (and retries the rewrite) until the document states
/// something else. Found in the final review: the second start of such a
/// server silently went back to loopback after the notice had said otherwise.
pub fn migrate(document: &Path, state_dir: &Path, parsed_bind: Option<&str>) -> Migration {
    let marker = state_dir.join(MARKER);
    let pending = match fs::read_to_string(&marker) {
        Ok(text) => text.starts_with(PENDING),
        Err(_) if marker.exists() => false,
        Err(_) => true,
    };
    if !pending {
        return Migration::NotNeeded;
    }
    let outcome = if parsed_bind == Some(OLD_DEFAULT) {
        rewrite(document).unwrap_or_else(|reason| Migration::BindOnly { reason })
    } else {
        Migration::NotNeeded
    };
    // The notice is one-time for a rewrite; a BindOnly run leaves the
    // migration pending so the next start binds the new default too. A
    // marker that cannot be written only means the check runs again at the
    // next start.
    if let Some(parent) = marker.parent() {
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent);
    }
    let text = match &outcome {
        Migration::BindOnly { .. } => format!(
            "{PENDING}: the document still states {OLD_DEFAULT} and could not be rewritten; \
             each start binds {NEW_DEFAULT}\n"
        ),
        _ => format!("inference bind migration ran; the new default is {NEW_DEFAULT}\n"),
    };
    let _ = fs::write(&marker, text);
    outcome
}

/// The first word of a marker whose migration could not rewrite the document.
const PENDING: &str = "pending";

fn rewrite(document: &Path) -> Result<Migration, String> {
    let text = fs::read_to_string(document).map_err(|e| format!("cannot read it: {e}"))?;
    // Design §9: only the one value changes, so comments and layout survive.
    // A second occurrence (a comment, another listener) makes the rewrite
    // ambiguous, and an ambiguous administrator document is never edited.
    if text.matches(OLD_DEFAULT).count() != 1 {
        return Err(format!(
            "{OLD_DEFAULT} does not occur exactly once in the document"
        ));
    }
    let mode = fs::metadata(document)
        .map_err(|e| format!("cannot read it: {e}"))?
        .permissions()
        .mode()
        & 0o7777;
    let name = document
        .file_name()
        .ok_or("the path names no file")?
        .to_string_lossy()
        .into_owned();
    let dir = match document.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let backup = dir.join(format!("{name}.pre-0.1.0"));
    let temporary = dir.join(format!(".{name}.migrating"));
    let write = |path: &Path, body: &str| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)?;
        // The process umask may have narrowed the creation mode.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(body.as_bytes())?;
        file.sync_all()
    };
    // A backup left by an earlier interrupted attempt is the original already.
    if !backup.exists() {
        write(&backup, &text).map_err(|e| format!("cannot keep a backup: {e}"))?;
    }
    let _ = fs::remove_file(&temporary);
    write(&temporary, &text.replacen(OLD_DEFAULT, NEW_DEFAULT, 1)).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("cannot write: {e}")
    })?;
    fs::rename(&temporary, document).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("cannot replace it: {e}")
    })?;
    // The rename is durable once the directory is.
    let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    Ok(Migration::Rewritten { backup })
}

/// The one-time notice for `outcome` (design §9), printed by the role to stderr
/// on the start that migrated. `None` when nothing was migrated.
pub fn notice(document: &Path, outcome: &Migration) -> Option<String> {
    let path = document.display();
    match outcome {
        Migration::NotNeeded => None,
        Migration::Rewritten { backup } => Some(format!(
            "NOTICE: mllm 0.1.0 serves inference on all interfaces: {NEW_DEFAULT} (was {OLD_DEFAULT}).\n\
             The API key is still required. Configuration updated: {path} (previous copy: {}).\n\
             To keep inference local, start with --listen {OLD_DEFAULT} or set listeners.inference.bind.",
            backup.display()
        )),
        Migration::BindOnly { reason } => Some(format!(
            "NOTICE: mllm 0.1.0 serves inference on all interfaces: {NEW_DEFAULT} (was {OLD_DEFAULT}).\n\
             The API key is still required. {path} was not changed ({reason}); edit listeners.inference.bind there.\n\
             To keep inference local, start with --listen {OLD_DEFAULT}."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const DOC: &str = "{\n  \"listeners\": {\n    \"management\": {\"bind\": \"127.0.0.1:7443\"},\n    \"inference\": {\"bind\": \"127.0.0.1:8443\", \"authentication\": \"api_key\"}\n  }\n}\n";

    fn setup(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("server.yaml");
        fs::write(&doc, text).unwrap();
        (dir, doc)
    }

    // T02 / owner decision 1: the old default is rewritten once, with a backup and a marker.
    #[test]
    fn the_old_default_is_migrated_once() {
        let (dir, doc) = setup(DOC);
        let outcome = migrate(&doc, dir.path(), Some(OLD_DEFAULT));
        let Migration::Rewritten { backup } = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(fs::read_to_string(backup).unwrap(), DOC);
        let after = fs::read_to_string(&doc).unwrap();
        assert_eq!(after, DOC.replace("127.0.0.1:8443", "0.0.0.0:8443"));
        assert!(after.contains("\"api_key\""));
        assert!(dir.path().join(MARKER).exists());
        assert!(notice(&doc, &outcome)
            .unwrap()
            .starts_with("NOTICE: mllm 0.1.0 serves inference on all interfaces"));
        // A second start, even after the operator sets loopback back, does nothing.
        fs::write(&doc, DOC).unwrap();
        assert!(matches!(
            migrate(&doc, dir.path(), Some(OLD_DEFAULT)),
            Migration::NotNeeded
        ));
        assert_eq!(fs::read_to_string(&doc).unwrap(), DOC);
    }

    // T02 (design §9): the notice is the spec's text, and the backup keeps the
    // document's mode.
    #[test]
    fn the_notice_and_backup_are_as_specified() {
        let (dir, doc) = setup(DOC);
        fs::set_permissions(&doc, fs::Permissions::from_mode(0o640)).unwrap();
        let outcome = migrate(&doc, dir.path(), Some(OLD_DEFAULT));
        let Migration::Rewritten { backup } = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(backup, &dir.path().join("server.yaml.pre-0.1.0"));
        for file in [backup, &doc] {
            let mode = fs::metadata(file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640, "{}", file.display());
        }
        assert!(!dir.path().join(".server.yaml.migrating").exists());
        let d = doc.display();
        let b = backup.display();
        assert_eq!(
            notice(&doc, &outcome).unwrap(),
            format!(
                "NOTICE: mllm 0.1.0 serves inference on all interfaces: 0.0.0.0:8443 (was 127.0.0.1:8443).\n\
                 The API key is still required. Configuration updated: {d} (previous copy: {b}).\n\
                 To keep inference local, start with --listen 127.0.0.1:8443 or set listeners.inference.bind."
            )
        );
        assert_eq!(notice(&doc, &Migration::NotNeeded), None);
    }

    // T03: an operator's own address is never migrated.
    #[test]
    fn other_addresses_are_left_alone() {
        for bind in ["127.0.0.1:9000", "100.64.0.5:8443", "0.0.0.0:8443"] {
            let (dir, doc) = setup(&DOC.replace("127.0.0.1:8443", bind));
            assert!(
                matches!(migrate(&doc, dir.path(), Some(bind)), Migration::NotNeeded),
                "{bind}"
            );
        }
    }

    // T03 (design §9): the first start of this release records the migration
    // whatever it found, so loopback set afterwards is the operator's choice.
    #[test]
    fn loopback_chosen_after_the_first_start_is_kept() {
        let fresh = DOC.replace("127.0.0.1:8443", "0.0.0.0:8443");
        let (dir, doc) = setup(&fresh);
        assert!(matches!(
            migrate(&doc, dir.path(), Some("0.0.0.0:8443")),
            Migration::NotNeeded
        ));
        assert!(dir.path().join(MARKER).exists());
        fs::write(&doc, DOC).unwrap();
        assert!(matches!(
            migrate(&doc, dir.path(), Some(OLD_DEFAULT)),
            Migration::NotNeeded
        ));
        assert_eq!(fs::read_to_string(&doc).unwrap(), DOC);
    }

    // Review focus 3: ambiguous text is not rewritten; the run still serves on 0.0.0.0.
    #[test]
    fn ambiguous_text_binds_only() {
        let text = format!("# was 127.0.0.1:8443\n{DOC}");
        let (dir, doc) = setup(&text);
        let outcome = migrate(&doc, dir.path(), Some(OLD_DEFAULT));
        assert!(matches!(outcome, Migration::BindOnly { .. }), "{outcome:?}");
        assert_eq!(fs::read_to_string(&doc).unwrap(), text);
        assert!(!dir.path().join("server.yaml.pre-0.1.0").exists());
        let said = notice(&doc, &outcome).unwrap();
        assert!(
            said.contains("was not changed") && said.contains("edit listeners.inference.bind"),
            "{said}"
        );
        // Nothing was rewritten, so the next start binds the new default
        // again rather than silently returning to loopback.
        assert!(matches!(
            migrate(&doc, dir.path(), Some(OLD_DEFAULT)),
            Migration::BindOnly { .. }
        ));
        // Once the operator states another address, it is theirs.
        assert!(matches!(
            migrate(&doc, dir.path(), Some("127.0.0.1:9000")),
            Migration::NotNeeded
        ));
        assert!(matches!(
            migrate(&doc, dir.path(), Some(OLD_DEFAULT)),
            Migration::NotNeeded
        ));
    }

    // T02 T03 (final review I1): a packaged server whose `/etc` is read-only
    // cannot persist the rewrite. Every start keeps binding the new default
    // (with the notice) instead of the second start silently returning to
    // loopback; a later writable start completes the migration once.
    #[test]
    fn a_read_only_document_keeps_the_new_default_across_starts() {
        let (dir, _) = setup(DOC);
        let config = dir.path().join("etc");
        fs::create_dir(&config).unwrap();
        let doc = config.join("server.yaml");
        fs::write(&doc, DOC).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o500)).unwrap();
        for start in 0..3 {
            let outcome = migrate(&doc, dir.path(), Some(OLD_DEFAULT));
            assert!(
                matches!(outcome, Migration::BindOnly { .. }),
                "start {start}: {outcome:?}"
            );
            assert!(notice(&doc, &outcome).is_some());
        }
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read_to_string(&doc).unwrap(), DOC);
        assert!(matches!(
            migrate(&doc, dir.path(), Some(OLD_DEFAULT)),
            Migration::Rewritten { .. }
        ));
        assert!(matches!(
            migrate(&doc, dir.path(), Some(NEW_DEFAULT)),
            Migration::NotNeeded
        ));
    }

    // Review focus 3: an unwritable directory never corrupts the document.
    #[test]
    fn an_unwritable_document_binds_only() {
        let (dir, doc) = setup(DOC);
        let config = dir.path().join("ro");
        fs::create_dir(&config).unwrap();
        let ro_doc = config.join("server.yaml");
        fs::write(&ro_doc, DOC).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = migrate(&ro_doc, dir.path(), Some(OLD_DEFAULT));
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(outcome, Migration::BindOnly { .. }), "{outcome:?}");
        assert_eq!(fs::read_to_string(&ro_doc).unwrap(), DOC);
        let _ = doc;
    }
}
