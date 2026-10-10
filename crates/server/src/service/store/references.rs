use super::*;

fn project(target: &FileTarget) -> &str {
  match target {
    FileTarget::Run { scope, .. } => &scope.project_id,
    FileTarget::Input { project_id, .. } => project_id,
  }
}

fn validate_reference(target: &FileTarget) -> ApiResult<()> {
  validate_target(target).map_err(bad)?;
  if let FileTarget::Run { path, .. } = target {
    crate::run_artifacts::validate_path(path).map_err(bad)?;
  }
  Ok(())
}

impl<S: ObjectStorage> Store<S> {
  pub(super) fn reference_file(
    &self,
    source: FileTarget,
    target: FileTarget,
    size: u64,
    sha256: String,
  ) -> ApiResult<Response> {
    validate_reference(&source)?;
    validate_reference(&target)?;
    if project(&source) != project(&target) {
      return Err(ApiError::new(
        400,
        "file references must stay within one project",
      ));
    }
    let mut db = self.db()?;
    let transaction = db.transaction().map_err(database)?;
    run_retention::ensure_target_available(&transaction, &source)?;
    run_retention::ensure_target_available(&transaction, &target)?;
    let (stored, key, sequence): (String, String, i64) = transaction
      .query_row(
        "SELECT record,object_key,sequence FROM files WHERE target=?1",
        [target_json(&source)?],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
      )
      .optional()
      .map_err(database)?
      .ok_or_else(|| ApiError::new(404, "completed source object is missing"))?;
    let mut record: FileRecord =
      serde_json::from_str(&stored).map_err(|_| ApiError::new(500, "invalid stored artifact"))?;
    if record.target != source
      || !matches!(record.storage, FileStorage::Object)
      || record.size != size
      || record.sha256.as_deref() != Some(&sha256)
    {
      return Err(ApiError::new(
        409,
        "source object does not match the pinned identity",
      ));
    }
    record.target = target.clone();
    let encoded = target_json(&target)?;
    tracking::reject_managed(&transaction, &target)?;
    let existing: Option<(String, String)> = transaction
      .query_row(
        "SELECT record,object_key FROM files WHERE target=?1",
        [&encoded],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database)?;
    if let Some((stored, existing_key)) = existing {
      let previous: FileRecord =
        serde_json::from_str(&stored).map_err(|_| ApiError::new(500, "invalid stored artifact"))?;
      if previous.size != size || previous.sha256 != record.sha256 || existing_key != key {
        return Err(ApiError::new(
          409,
          "reference destination already exists with another object",
        ));
      }
    } else {
      let live: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM streams WHERE target=?1) OR EXISTS(SELECT 1 FROM uploads WHERE target=?1 AND complete=0)",
        [&encoded], |row| row.get(0)).map_err(database)?;
      if live {
        return Err(ApiError::new(
          409,
          "reference destination has an active upload or stream",
        ));
      }
      transaction
        .execute(
          "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,?3,?4)",
          params![
            encoded,
            serde_json::to_string(&record)
              .map_err(|_| ApiError::new(500, "cannot encode artifact"))?,
            key,
            sequence
          ],
        )
        .map_err(database)?;
      if let FileTarget::Run { scope, .. } = &target {
        record_run_activity(&transaction, scope)?;
      }
      record_storage_publication(&transaction, &target)?;
    }
    transaction.commit().map_err(database)?;
    Ok(Response::File { file: record })
  }
}

#[cfg(test)]
mod tests {
  use super::super::tests::{MockStorage, scope};
  use super::*;

  fn publish(store: &Store<MockStorage>, storage: &MockStorage, target: FileTarget, id: &str) {
    store
      .execute(Request::BeginUpload {
        upload_id: id.into(),
        target,
        size: 7,
        sha256: "a".repeat(64),
      })
      .unwrap();
    store
      .execute(Request::RecordPart {
        upload_id: id.into(),
        part: CompletedPart {
          part_number: 1,
          etag: "part".into(),
        },
      })
      .unwrap();
    storage.stage(id, 7);
    store
      .execute(Request::CompleteUpload {
        upload_id: id.into(),
      })
      .unwrap();
  }

  #[test]
  fn references_share_objects_and_survive_reopen_without_transfers() {
    let directory = tempfile::tempdir().unwrap();
    let storage = MockStorage::default();
    let store = Store::open(directory.path(), storage.clone()).unwrap();
    let source = FileTarget::Input {
      project_id: "project".into(),
      input_id: "historical".into(),
    };
    publish(&store, &storage, source.clone(), "original");
    let output = FileTarget::Run {
      scope: scope(),
      path: "outputs/datasets/paired/source.tar.gz".into(),
    };
    let input = FileTarget::Input {
      project_id: "project".into(),
      input_id: "named-asset".into(),
    };
    let dashboard_source = DashboardSource {
      project_id: "project".into(),
      origin: "worker".into(),
    };
    assert_eq!(
      store
        .dashboard_updates(Some(&dashboard_source), &[])
        .unwrap()["storage_revision"],
      "1"
    );
    for (from, to, revision) in [
      (source.clone(), output.clone(), "2"),
      (output.clone(), input.clone(), "3"),
      (output.clone(), input.clone(), "3"),
    ] {
      store
        .execute(Request::ReferenceFile {
          source: from,
          target: to,
          size: 7,
          sha256: "a".repeat(64),
        })
        .unwrap();
      assert_eq!(
        store
          .dashboard_updates(Some(&dashboard_source), &[])
          .unwrap()["storage_revision"],
        revision,
        "new references refresh Storage, while idempotent retries do not"
      );
    }
    let url = |target| match store.execute(Request::DownloadUrl { target }).unwrap() {
      Response::Url { url } => url,
      _ => panic!("URL"),
    };
    assert_eq!(url(source.clone()), url(output.clone()));
    assert_eq!(url(source), url(input.clone()));
    let Response::Files { files } = store
      .execute(Request::ListFiles { scope: scope() })
      .unwrap()
    else {
      panic!("files")
    };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].target, output);
    drop(store);
    let reopened = Store::open(directory.path(), storage).unwrap();
    assert_eq!(reopened.file(&input).unwrap().sha256, Some("a".repeat(64)));
    assert_eq!(
      reopened
        .dashboard_updates(Some(&dashboard_source), &[])
        .unwrap()["storage_revision"],
      "3"
    );
  }

  #[test]
  fn references_reject_identity_changes_foreign_projects_and_managed_paths() {
    let directory = tempfile::tempdir().unwrap();
    let storage = MockStorage::default();
    let store = Store::open(directory.path(), storage.clone()).unwrap();
    let source = FileTarget::Input {
      project_id: "project".into(),
      input_id: "source".into(),
    };
    publish(&store, &storage, source.clone(), "original");
    let input = FileTarget::Input {
      project_id: "project".into(),
      input_id: "asset".into(),
    };
    assert_eq!(
      store
        .reference_file(source.clone(), input.clone(), 8, "a".repeat(64))
        .unwrap_err()
        .status,
      409
    );
    assert_eq!(
      store
        .reference_file(source.clone(), input.clone(), 7, "b".repeat(64))
        .unwrap_err()
        .status,
      409
    );
    assert_eq!(
      store
        .reference_file(
          source.clone(),
          FileTarget::Input {
            project_id: "foreign".into(),
            input_id: "asset".into()
          },
          7,
          "a".repeat(64)
        )
        .unwrap_err()
        .status,
      400
    );
    for path in [
      "run-state.json",
      "result.zip",
      "outputs/../secret",
      "outputs/.expri-artifacts.json",
    ] {
      assert_eq!(
        store
          .reference_file(
            source.clone(),
            FileTarget::Run {
              scope: scope(),
              path: path.into()
            },
            7,
            "a".repeat(64)
          )
          .unwrap_err()
          .status,
        400
      );
    }
    publish(&store, &storage, input.clone(), "different-key");
    assert_eq!(
      store
        .reference_file(source, input, 7, "a".repeat(64))
        .unwrap_err()
        .status,
      409
    );
  }
}
