//! Positive proofs reused only within one explicitly borrowed, unchanged transaction.
use super::*;
use std::cell::RefCell;
use std::collections::BTreeSet;

pub(super) struct ReadValidation<'tx, 'conn> {
    tx: &'tx Transaction<'conn>,
    changes: u64,
    schema_version: Option<i64>,
    temp_schema_version: Option<i64>,
    proven: RefCell<BTreeSet<String>>,
    active: RefCell<BTreeSet<String>>,
    bytes: std::cell::Cell<usize>,
}

impl<'tx, 'conn> ReadValidation<'tx, 'conn> {
    pub(super) fn new(tx: &'tx Transaction<'conn>) -> Self {
        Self {
            tx,
            changes: tx.total_changes(),
            schema_version: tx
                .query_row("PRAGMA main.schema_version", [], |r| r.get(0))
                .ok(),
            temp_schema_version: tx
                .query_row("PRAGMA temp.schema_version", [], |r| r.get(0))
                .ok(),
            proven: RefCell::new(BTreeSet::new()),
            active: RefCell::new(BTreeSet::new()),
            bytes: std::cell::Cell::new(0),
        }
    }

    pub(super) fn prove(
        &self,
        tx: &Transaction<'_>,
        key: String,
        validate: impl FnOnce() -> Result<(), LifecycleError>,
    ) -> Result<(), LifecycleError> {
        self.unchanged(tx)?;
        if self.proven.borrow().contains(&key) {
            return Ok(());
        }
        if self.active.borrow().len() >= 128
            || self.proven.borrow().len() + self.active.borrow().len() >= 4096
            || self
                .bytes
                .get()
                .checked_add(key.len())
                .is_none_or(|n| n > 8 * 1024 * 1024)
            || !self.active.borrow_mut().insert(key.clone())
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        self.bytes.set(self.bytes.get() + key.len());
        let result = validate();
        self.active.borrow_mut().remove(&key);
        if let Err(error) = result.and_then(|_| self.unchanged(tx)) {
            self.bytes.set(self.bytes.get() - key.len());
            return Err(error);
        }
        self.proven.borrow_mut().insert(key);
        Ok(())
    }

    fn unchanged(&self, tx: &Transaction<'_>) -> Result<(), LifecycleError> {
        if !std::ptr::eq(self.tx, tx)
            || self.changes != tx.total_changes()
            || self.schema_version.is_none()
            || self.schema_version
                != Some(tx.query_row("PRAGMA main.schema_version", [], |r| r.get(0))?)
            || self.temp_schema_version.is_none()
            || self.temp_schema_version
                != Some(tx.query_row("PRAGMA temp.schema_version", [], |r| r.get(0))?)
        {
            return Err(LifecycleError::Conflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_proof_cannot_cross_schema_only_writes() {
        for write in [
            "ALTER TABLE source RENAME TO replaced;",
            "CREATE TEMP VIEW source AS SELECT 2 AS value;",
        ] {
            let mut connection = rusqlite::Connection::open_in_memory().unwrap();
            connection
                .execute_batch("CREATE TABLE source(value INTEGER); INSERT INTO source VALUES(1);")
                .unwrap();
            let tx = connection.transaction().unwrap();
            let read = ReadValidation::new(&tx);
            let check = || {
                let value: i64 = tx.query_row("SELECT value FROM source", [], |r| r.get(0))?;
                if value == 1 {
                    Ok(())
                } else {
                    Err(LifecycleError::Conflict)
                }
            };
            read.prove(&tx, "source".into(), check).unwrap();
            tx.execute_batch(write).unwrap();
            assert!(read.prove(&tx, "source".into(), check).is_err());
        }
    }

    #[test]
    fn positive_proof_cannot_cross_writes_transactions_or_failed_validation() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE source(value INTEGER); INSERT INTO source VALUES(1);")
            .unwrap();
        {
            let tx = connection.transaction().unwrap();
            let read = ReadValidation::new(&tx);
            let check = || {
                let value: i64 = tx.query_row("SELECT value FROM source", [], |r| r.get(0))?;
                if value == 1 {
                    Ok(())
                } else {
                    Err(LifecycleError::Conflict)
                }
            };
            read.prove(&tx, "source".into(), check).unwrap();
            tx.execute("UPDATE source SET value=2", []).unwrap();
            assert!(read.prove(&tx, "source".into(), check).is_err());
            let fresh = ReadValidation::new(&tx);
            assert!(fresh.prove(&tx, "source".into(), check).is_err());
            assert!(fresh.prove(&tx, "source".into(), check).is_err());
            tx.commit().unwrap();
        }
        let tx = connection.transaction().unwrap();
        let read = ReadValidation::new(&tx);
        assert!(
            read.prove(&tx, "source".into(), || {
                let value: i64 = tx.query_row("SELECT value FROM source", [], |r| r.get(0))?;
                if value == 1 {
                    Ok(())
                } else {
                    Err(LifecycleError::Conflict)
                }
            })
            .is_err()
        );
    }
}
