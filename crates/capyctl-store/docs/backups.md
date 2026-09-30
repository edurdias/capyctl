# Backup procedure (F0)

The server's durable state lives in a single embedded SQLite database
(`srv.sqlite3`, WAL mode) inside the owner-protected state directory
(0700 directory, 0600 files).

## Consistent copy

Use SQLite's built-in backup API so the copy is consistent even while
the live store is accepting writes:

```sh
sqlite3 /path/to/state_dir/srv.sqlite3 ".backup '/path/to/state_dir/backups/srv-YYYYMMDDTHHMMSSZ.sqlite3'"
```

- Create the backup **inside the same protected state directory** as
  the live store, never in a world-readable location.
- Immediately `chmod 0600` the backup file (and `chmod 0700` the
  backup directory if it is a subdirectory of the state dir).
- A backup inherits the store's content rule: **no secrets and no
  inference bodies** — the schema stores control-plane metadata only
  (`journal_entries.evidence` holds evidence, never payloads).
- Verify after copying: `sqlite3 <backup> "PRAGMA integrity_check;"`
  must return `ok`.

## Restore

Stop the server, replace the live file (and remove stale `-wal` /
`-shm` sidecars of the replaced file) with the verified backup, keep
0600 permissions, and restart. Migrations run forward-only at startup,
so only restore a backup whose schema version is not newer than the
running binary.

## Retention

Retention and rotation of old backup copies are **explicitly deferred
to a later milestone**; F0 documents only the copy procedure and its
permission posture.