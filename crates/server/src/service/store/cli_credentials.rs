//! Only opaque credential digests and their secret-bound epoch are durable.

use super::*;

pub(super) const ACTIVE_CREDENTIAL_LIMIT: usize = 256;

pub(super) fn initialize(db: &Connection) -> crate::error::Result<()> {
  db.execute_batch(
    "CREATE TABLE IF NOT EXISTS cli_credentials (
    digest BLOB PRIMARY KEY CHECK(length(digest)=32),
    epoch BLOB NOT NULL CHECK(length(epoch)=32),
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    revoked_at_ms INTEGER);
    CREATE INDEX IF NOT EXISTS cli_credentials_active ON cli_credentials(epoch,revoked_at_ms);
    CREATE TABLE IF NOT EXISTS cli_credential_state (
      id INTEGER PRIMARY KEY CHECK(id=1),
      epoch BLOB CHECK(epoch IS NULL OR length(epoch)=32));",
  )
  .map_err(|_| crate::error::ExpriError::Message("cannot initialize CLI credentials".into()))
}

fn set_epoch(db: &Connection, epoch: Option<&[u8; 32]>) -> ApiResult<()> {
  let previous: Option<Option<Vec<u8>>> = db
    .query_row(
      "SELECT epoch FROM cli_credential_state WHERE id=1",
      [],
      |row| row.get(0),
    )
    .optional()
    .map_err(database)?;
  let next = epoch.map(|value| value.as_slice());
  if previous
    .as_ref()
    .is_some_and(|previous| previous.as_deref() == next)
  {
    return Ok(());
  }
  // A rotation is irreversible: restoring an older password or API key must
  // not resurrect credentials from before that revocation event.
  db.execute("DELETE FROM cli_credentials", [])
    .map_err(database)?;
  db.execute("INSERT INTO cli_credential_state(id,epoch) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET epoch=excluded.epoch", [next]).map_err(database)?;
  Ok(())
}

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn set_cli_credential_epoch(
    &self,
    epoch: Option<&[u8; 32]>,
  ) -> ApiResult<()> {
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    set_epoch(&tx, epoch)?;
    tx.commit().map_err(database)
  }

  pub(in crate::service) fn issue_cli_credential(
    &self,
    digest: &[u8; 32],
    epoch: &[u8; 32],
    at_ms: i64,
  ) -> ApiResult<()> {
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    set_epoch(&tx, Some(epoch))?;
    // Rotation invalidates old epochs. Prune them and already-revoked entries
    // before counting, without evicting a credential held by another client.
    tx.execute("DELETE FROM cli_credentials WHERE epoch<>?1 OR revoked_at_ms IS NOT NULL OR (expires_at_ms IS NOT NULL AND expires_at_ms<=?2)", params![epoch.as_slice(), at_ms]).map_err(database)?;
    let active: usize = tx
      .query_row(
        "SELECT COUNT(*) FROM cli_credentials WHERE epoch=?1 AND revoked_at_ms IS NULL",
        [epoch.as_slice()],
        |row| row.get(0),
      )
      .map_err(database)?;
    if active >= ACTIVE_CREDENTIAL_LIMIT {
      return Err(ApiError::new(
        409,
        "CLI credential limit reached; log out an existing client before signing in",
      ));
    }
    tx.execute(
      "INSERT INTO cli_credentials(digest,epoch,created_at_ms) VALUES(?1,?2,?3)",
      params![digest.as_slice(), epoch.as_slice(), at_ms],
    )
    .map_err(database)?;
    tx.commit().map_err(database)
  }

  pub(in crate::service) fn accepts_cli_credential(
    &self,
    digest: &[u8; 32],
    epoch: &[u8; 32],
    at_ms: i64,
  ) -> ApiResult<bool> {
    self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM cli_credentials WHERE digest=?1 AND epoch=?2 AND epoch=(SELECT epoch FROM cli_credential_state WHERE id=1) AND revoked_at_ms IS NULL AND (expires_at_ms IS NULL OR expires_at_ms>?3))", params![digest.as_slice(), epoch.as_slice(), at_ms], |row| row.get(0)).map_err(database)
  }

  pub(in crate::service) fn revoke_cli_credential(
    &self,
    digest: &[u8; 32],
    epoch: &[u8; 32],
    at_ms: i64,
  ) -> ApiResult<bool> {
    Ok(self.db()?.execute("UPDATE cli_credentials SET revoked_at_ms=?3 WHERE digest=?1 AND epoch=?2 AND epoch=(SELECT epoch FROM cli_credential_state WHERE id=1) AND revoked_at_ms IS NULL", params![digest.as_slice(), epoch.as_slice(), at_ms]).map_err(database)? != 0)
  }
}

#[cfg(test)]
mod tests;
