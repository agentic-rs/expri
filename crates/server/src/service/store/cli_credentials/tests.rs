use sha2::{Digest, Sha256};

use super::*;
use crate::service::store::tests::{MockStorage, scope};

fn digest(number: usize) -> [u8; 32] {
  Sha256::digest(number.to_be_bytes()).into()
}

#[test]
fn credential_limit_does_not_evict_clients_and_revocation_releases_one_slot() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let epoch = [4; 32];
  for number in 0..ACTIVE_CREDENTIAL_LIMIT {
    store
      .issue_cli_credential(&digest(number), &epoch, 100)
      .unwrap();
  }
  assert_eq!(
    store
      .issue_cli_credential(&digest(256), &epoch, 101)
      .unwrap_err()
      .status,
    409
  );
  assert!(
    store
      .accepts_cli_credential(&digest(0), &epoch, i64::MAX)
      .unwrap()
  );
  assert!(
    store
      .accepts_cli_credential(&digest(255), &epoch, i64::MAX)
      .unwrap()
  );
  assert!(
    store
      .revoke_cli_credential(&digest(0), &epoch, 102)
      .unwrap()
  );
  assert!(
    !store
      .revoke_cli_credential(&digest(0), &epoch, 103)
      .unwrap()
  );
  assert!(
    !store
      .accepts_cli_credential(&digest(0), &epoch, 104)
      .unwrap()
  );
  store
    .issue_cli_credential(&digest(256), &epoch, 104)
    .unwrap();
  assert!(
    store
      .accepts_cli_credential(&digest(255), &epoch, 105)
      .unwrap()
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM cli_credentials", [], |row| row
        .get::<_, usize>(0))
      .unwrap(),
    256
  );
}

#[test]
fn epoch_rotation_invalidates_previous_credentials_and_keeps_only_opaque_metadata() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let first = [1; 32];
  let second = [2; 32];
  store.issue_cli_credential(&digest(1), &first, 10).unwrap();
  assert!(
    !store
      .accepts_cli_credential(&digest(1), &second, 11)
      .unwrap()
  );
  assert!(
    !store
      .revoke_cli_credential(&digest(1), &second, 11)
      .unwrap()
  );
  store.issue_cli_credential(&digest(2), &second, 12).unwrap();
  assert!(
    !store
      .accepts_cli_credential(&digest(1), &first, 13)
      .unwrap()
  );
  let db = store.db().unwrap();
  let (stored_digest, stored_epoch, created, expiry, revoked): (
    Vec<u8>,
    Vec<u8>,
    i64,
    Option<i64>,
    Option<i64>,
  ) = db
    .query_row(
      "SELECT digest,epoch,created_at_ms,expires_at_ms,revoked_at_ms FROM cli_credentials",
      [],
      |row| {
        Ok((
          row.get(0)?,
          row.get(1)?,
          row.get(2)?,
          row.get(3)?,
          row.get(4)?,
        ))
      },
    )
    .unwrap();
  assert_eq!(stored_digest, digest(2));
  assert_eq!(stored_epoch, second);
  assert_eq!(created, 12);
  assert!(expiry.is_none());
  assert!(revoked.is_none());
}

#[test]
fn startup_rotation_and_dashboard_disable_cannot_resurrect_old_credentials() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let first = [1; 32];
  let second = [2; 32];
  store.issue_cli_credential(&digest(1), &first, 100).unwrap();
  store.set_cli_credential_epoch(Some(&first)).unwrap();
  assert!(
    store
      .accepts_cli_credential(&digest(1), &first, 101)
      .unwrap()
  );
  store.set_cli_credential_epoch(Some(&second)).unwrap();
  store.set_cli_credential_epoch(Some(&first)).unwrap();
  assert!(
    !store
      .accepts_cli_credential(&digest(1), &first, 102)
      .unwrap()
  );
  store.issue_cli_credential(&digest(2), &first, 103).unwrap();
  store.set_cli_credential_epoch(None).unwrap();
  store.set_cli_credential_epoch(Some(&first)).unwrap();
  assert!(
    !store
      .accepts_cli_credential(&digest(2), &first, 104)
      .unwrap()
  );
}

#[test]
fn additive_credential_table_preserves_schema_seven_run_data_across_reopen() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  store
    .execute(Request::AppendStream {
      scope: scope(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"preserved\n"),
    })
    .unwrap();
  store
    .db()
    .unwrap()
    .execute_batch("DROP TABLE cli_credentials; PRAGMA user_version=7;")
    .unwrap();
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let Response::Files { files } = store
    .execute(Request::ListFiles { scope: scope() })
    .unwrap()
  else {
    panic!("expected files")
  };
  assert_eq!(files.len(), 1);
  store
    .issue_cli_credential(&digest(1), &[3; 32], 100)
    .unwrap();
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert!(
    store
      .accepts_cli_credential(&digest(1), &[3; 32], i64::MAX)
      .unwrap()
  );
}

#[test]
fn concurrent_issuance_cannot_exceed_the_active_credential_limit() {
  let directory = tempfile::tempdir().unwrap();
  let store = std::sync::Arc::new(Store::open(directory.path(), MockStorage::default()).unwrap());
  let epoch = [7; 32];
  for number in 0..ACTIVE_CREDENTIAL_LIMIT - 1 {
    store
      .issue_cli_credential(&digest(number), &epoch, 100)
      .unwrap();
  }
  let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
  let workers: Vec<_> = [255, 256]
    .into_iter()
    .map(|number| {
      let store = store.clone();
      let barrier = barrier.clone();
      std::thread::spawn(move || {
        barrier.wait();
        store
          .issue_cli_credential(&digest(number), &epoch, 101)
          .map_err(|error| error.status)
      })
    })
    .collect();
  barrier.wait();
  let results: Vec<_> = workers
    .into_iter()
    .map(|worker| worker.join().unwrap())
    .collect();
  assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
  assert_eq!(
    results.iter().filter(|result| **result == Err(409)).count(),
    1
  );
}
