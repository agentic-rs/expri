//! Materialize durable capture batches without holding the collector write lock.

use super::*;

impl LocalStore {
  pub fn begin_run_capture(
    &mut self,
    run_dir: &Path,
    run_id: &str,
    start_ms: i64,
  ) -> Result<String> {
    self.begin_capture(run_dir, run_id, start_ms, false, None)
  }

  pub fn begin_run_capture_segment(
    &mut self,
    run_dir: &Path,
    run_id: &str,
    start_ms: i64,
  ) -> Result<String> {
    self.begin_capture(run_dir, run_id, start_ms, true, None)
  }

  pub fn begin_run_capture_segment_reserved(
    &mut self,
    run_dir: &Path,
    run_id: &str,
    start_ms: i64,
    capture_id: &str,
  ) -> Result<String> {
    validate_component(capture_id)?;
    self.begin_capture(run_dir, run_id, start_ms, true, Some(capture_id))
  }

  fn begin_capture(
    &mut self,
    run_dir: &Path,
    run_id: &str,
    start_ms: i64,
    append: bool,
    reserved: Option<&str>,
  ) -> Result<String> {
    validate_component(run_id)?;
    if start_ms < 0 {
      return Err(rejection(400, "invalid run capture start"));
    }
    let database = Path::new(
      self
        .connection
        .path()
        .ok_or_else(|| rejection(400, "invalid profile path"))?,
    );
    let parent = fs::canonicalize(
      database
        .parent()
        .ok_or_else(|| rejection(400, "invalid profile path"))?,
    )?;
    let live = matches!(
      lock::try_lock_file_readonly(&parent.join("collector.lock"))?,
      LockAttempt::Busy
    );
    let (supported, empty): (bool, bool) = sql(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM system_run_capture_support p WHERE p.singleton=l.singleton AND p.sequence=l.sequence),l.sequence=0 FROM system_local l WHERE l.singleton=1", [], |row| Ok((row.get(0)?, row.get(1)?))))?;
    if !supported && (live || !empty) {
      return Err(rejection(
        409,
        "run capture requires an upgraded collector observation",
      ));
    }
    let run_dir = fs::canonicalize(run_dir)?;
    if run_dir.file_name().and_then(|name| name.to_str()) != Some(run_id) {
      return Err(rejection(
        400,
        "run capture directory has a different identity",
      ));
    }
    let capture_id = reserved
      .map(str::to_owned)
      .unwrap_or_else(|| format!("capture-{}", uuid::Uuid::new_v4().simple()));
    // Exclusive reservation precedes any registration. Existing user files are
    // never opened for writing or removed, even if they happen to be empty.
    let _stream_lease = match lock::try_lock_file(&run_dir.join(LEASE), true)? {
      LockAttempt::Acquired(lease) => lease,
      _ => return Err(rejection(409, "run system exporter is busy")),
    };
    let exists = run_dir.join(RUN_SYSTEM_PATH).try_exists()?;
    if !(append && exists) && fs::symlink_metadata(run_dir.join(CHECKPOINT)).is_ok() {
      return Err(rejection(409, "run system checkpoint already exists"));
    }
    let mut file = open_stream(&run_dir, !(append && exists))?;
    let file_identity = identity(&file)?;
    let segment_offset = if append && exists {
      let checkpoint: Checkpoint =
        serde_json::from_slice(&bounded_file(&run_dir.join(CHECKPOINT), 16 * 1024)?)?;
      if !checkpoint.closed
        || checkpoint.file_identity != file_identity
        || checkpoint.committed_bytes != file.metadata()?.len()
      {
        return Err(rejection(
          409,
          "previous run system segment is not fully closed",
        ));
      }
      let old = capture(&self.connection, &checkpoint.capture_id)?;
      if !old.closed || old.run_dir != run_dir || old.end_ms.is_none_or(|end| end > start_ms) {
        return Err(rejection(
          409,
          "run system continuation overlaps or precedes its previous segment",
        ));
      }
      file.seek(SeekFrom::End(0))?
    } else {
      0
    };
    let header = RunSystemRecord::Capture {
      schema_version: RUN_SYSTEM_SCHEMA_VERSION,
      capture_id: capture_id.clone(),
      machine_id: self.machine_id().into(),
      start_ms,
    };
    header.validate()?;
    let file_offset = segment_offset + serde_json::to_vec(&header)?.len() as u64 + 1;
    let machine_id = self.machine_id().to_owned();
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate),
    )?;
    sql(transaction.execute("INSERT INTO system_run_captures(capture_id,machine_id,run_id,run_dir,start_ms,file_offset,file_identity,segment_offset,header_pending) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1)", params![capture_id, machine_id, run_id, run_dir.to_string_lossy(), start_ms, file_offset, file_identity,segment_offset]))?;
    sql(transaction.execute(
      "UPDATE system_run_captures SET metadata_saved=1 WHERE run_dir=?1 AND closed=1",
      [run_dir.to_string_lossy().as_ref()],
    ))?;
    check_support(&transaction, &machine_id)?;
    backfill(&transaction, &capture_id)?;
    sql(transaction.commit())?;
    // Registration journals the exact next header before it can become visible
    // in the canonical stream. A crash or I/O failure can be repaired by any
    // later exporter without invalidating or guessing an earlier segment.
    let row = capture(&self.connection, &capture_id)?;
    let _ = self.materialize_capture_header(&mut file, &row);
    drop(_stream_lease);
    // Registration is already durable. A transient exporter error must not
    // orphan the capture by withholding its identity from run metadata.
    let _ = self.export_run_capture(&capture_id);
    Ok(capture_id)
  }

  pub fn close_run_capture(&mut self, capture_id: &str, end_ms: i64) -> Result<()> {
    let current = capture(&self.connection, capture_id)?;
    if end_ms < current.start_ms || (current.end_ms.is_some() && current.end_ms != Some(end_ms)) {
      return Err(rejection(
        409,
        "run capture end changed or precedes its start",
      ));
    }
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate),
    )?;
    sql(transaction.execute(
      "UPDATE system_run_captures SET end_ms=?2 WHERE capture_id=?1 AND closed=0",
      params![capture_id, end_ms],
    ))?;
    sql(transaction.execute("UPDATE system_run_captures SET coverage_lost=1 WHERE capture_id=?1 AND acknowledged=0 AND NOT EXISTS(SELECT 1 FROM system_run_capture_support p JOIN system_local l ON l.singleton=p.singleton WHERE p.sequence=l.sequence) AND EXISTS(SELECT 1 FROM system_local WHERE sequence>0)", [capture_id]))?;
    // Only unexported, now-ineligible reservations can be released. A committed
    // prefix remains immutable, including through later clock corrections.
    sql(transaction.execute(
      "DELETE FROM system_run_capture_minutes WHERE capture_id=?1 AND sequence IN (SELECT sequence FROM system_run_capture_samples WHERE capture_id=?1 AND observed_at_ms>=?2) AND sequence>(SELECT exported_sequence FROM system_run_captures WHERE capture_id=?1)",
      params![capture_id, end_ms],
    ))?;
    sql(transaction.execute(
      "DELETE FROM system_run_capture_samples WHERE capture_id=?1 AND observed_at_ms>=?2",
      params![capture_id, end_ms],
    ))?;
    sql(transaction.commit())
  }

  pub fn export_run_capture(&mut self, capture_id: &str) -> Result<bool> {
    let row = capture(&self.connection, capture_id)?;
    self.export_capture_from(row)
  }

  pub(super) fn export_capture_from(&mut self, row: Capture) -> Result<bool> {
    let capture_id = row.capture_id.clone();
    let lease_path = row.run_dir.join(LEASE);
    let _lease = match lock::try_lock_file(&lease_path, true)? {
      LockAttempt::Acquired(lease) => lease,
      LockAttempt::Busy => return Ok(false),
      LockAttempt::Missing => unreachable!(),
    };
    let current = capture(&self.connection, &capture_id)?;
    if current.header_pending {
      self.materialize_capture_header(&mut open_stream(&current.run_dir, false)?, &current)?;
    }
    if !sampling::seed(&self.connection, &current)? {
      return Ok(false);
    }
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Deferred),
    )?;
    let mut row = capture(&transaction, &capture_id)?;
    // This check must use the fresh row under the stream lease. A cached open
    // row can become historical while another exporter appends its successor.
    if row.closed {
      return Ok(false);
    }
    let records = {
      let mut statement = sql(transaction.prepare("SELECT sequence,inventory,sample FROM system_run_capture_samples WHERE capture_id=?1 ORDER BY sequence LIMIT ?2"))?;
      let mut query = sql(statement.query(params![capture_id, PAGE]))?;
      let mut records = Vec::new();
      let mut bytes = 0;
      while let Some(row) = sql(query.next())? {
        let record = (
          sql(row.get::<_, u64>(0))?,
          sql(row.get::<_, String>(1))?,
          sql(row.get::<_, String>(2))?,
        );
        let size = record.1.len() + record.2.len();
        if !records.is_empty() && bytes + size > PAGE_BYTES {
          break;
        }
        bytes += size;
        records.push(record);
      }
      records
    };
    sql(transaction.commit())?;
    let previous = row.clone();
    let mut file = open_stream(&row.run_dir, false)?;
    self.materialize_capture_header(&mut file, &row)?;
    validate_file(&file, &row)?;
    file.set_len(row.file_offset)?;
    file.seek(SeekFrom::Start(row.file_offset))?;
    let mut last_sequence = None;
    for (sequence, inventory, sample) in records {
      let inventory: InventoryRevision = decode(&inventory)?;
      if inventory.revision != row.inventory_revision {
        row.inventory_revision = inventory.revision;
        line(&mut file, &RunSystemRecord::Inventory { inventory })?;
      }
      line(
        &mut file,
        &RunSystemRecord::Sample {
          sample: decode(&sample)?,
        },
      )?;
      last_sequence = Some(sequence);
    }
    file.sync_all()?;
    row.file_offset = file.metadata()?.len();
    // Slow output files never hold the shared collector's SQLite write lock.
    // Only the checkpoint/delete commit is serialized with new observations.
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate),
    )?;
    let current = capture(&transaction, &capture_id)?;
    if current.file_offset != previous.file_offset
      || current.file_identity != previous.file_identity
      || current.end_ms != previous.end_ms
      || current.closed != previous.closed
    {
      return Err(rejection(
        409,
        "run capture scope changed during export; retry its durable outbox",
      ));
    }
    sql(transaction.execute(
      "UPDATE system_run_captures SET file_offset=?2,inventory_revision=?3 WHERE capture_id=?1",
      params![capture_id, row.file_offset, row.inventory_revision],
    ))?;
    if let Some(last) = last_sequence {
      sql(transaction.execute(
        "DELETE FROM system_run_capture_samples WHERE capture_id=?1 AND sequence<=?2",
        params![capture_id, last],
      ))?;
    }
    if let Some(last) = last_sequence {
      sql(transaction.execute("UPDATE system_run_captures SET exported_sequence=MAX(exported_sequence,?2) WHERE capture_id=?1", params![capture_id, last]))?;
    }
    sql(transaction.commit())?;
    save_checkpoint(&row)?;
    Ok(last_sequence.is_some())
  }

  fn materialize_capture_header(&self, file: &mut File, row: &Capture) -> Result<()> {
    if !row.header_pending {
      return Ok(());
    }
    let mut expected = serde_json::to_vec(&RunSystemRecord::Capture {
      schema_version: RUN_SYSTEM_SCHEMA_VERSION,
      capture_id: row.capture_id.clone(),
      machine_id: row.machine_id.clone(),
      start_ms: row.start_ms,
    })?;
    expected.push(b'\n');
    let length = file.metadata()?.len();
    if identity(file)? != row.file_identity
      || row.file_offset != row.segment_offset + expected.len() as u64
      || length < row.segment_offset
      || length > row.file_offset
    {
      return Err(rejection(
        409,
        "pending capture header no longer matches its reserved stream",
      ));
    }
    file.seek(SeekFrom::Start(row.segment_offset))?;
    let mut partial = Vec::new();
    file.read_to_end(&mut partial)?;
    if !expected.starts_with(&partial) {
      return Err(rejection(
        409,
        "pending capture header has unexpected bytes",
      ));
    }
    file.write_all(&expected[partial.len()..])?;
    file.sync_all()?;
    File::open(row.run_dir.join("outputs"))?.sync_all()?;
    save_checkpoint(row)?;
    sql(self.connection.execute(
      "UPDATE system_run_captures SET header_pending=0 WHERE capture_id=?1 AND header_pending=1",
      [&row.capture_id],
    ))?;
    Ok(())
  }

  pub(super) fn finish_capture(
    &mut self,
    capture_id: &str,
    complete: bool,
    warnings: Vec<String>,
  ) -> Result<()> {
    while self.export_run_capture(capture_id)? {}
    let row = capture(&self.connection, capture_id)?;
    let waiting = Instant::now();
    let _lease = loop {
      match lock::try_lock_file(&row.run_dir.join(LEASE), true)? {
        LockAttempt::Acquired(lease) => break lease,
        _ if waiting.elapsed() < Duration::from_secs(5) => thread::sleep(Duration::from_millis(25)),
        _ => return Err(rejection(409, "run capture exporter is busy")),
      }
    };
    let mut row = capture(&self.connection, capture_id)?;
    if row.closed {
      return save_checkpoint(&row);
    }
    let pending: u64 = sql(self.connection.query_row(
      "SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?1",
      [capture_id],
      |row| row.get(0),
    ))?;
    if pending > 0 {
      return Err(rejection(409, "run capture still has pending observations"));
    }
    let previous = row.clone();
    let end_ms = row
      .end_ms
      .ok_or_else(|| rejection(400, "run capture has no end"))?;
    let mut file = open_stream(&row.run_dir, false)?;
    validate_file(&file, &row)?;
    file.set_len(row.file_offset)?;
    file.seek(SeekFrom::Start(row.file_offset))?;
    let complete = complete && !row.coverage_lost;
    let warnings = if row.coverage_lost {
      vec!["The collector did not retain every observation during this run. Restart an upgraded collector before starting another run.".into()]
    } else {
      warnings
    };
    line(
      &mut file,
      &RunSystemRecord::End {
        end_ms,
        complete,
        warnings: warnings.clone(),
      },
    )?;
    file.sync_all()?;
    row.file_offset = file.metadata()?.len();
    row.closed = true;
    row.complete = complete;
    row.warnings = warnings;
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate),
    )?;
    let current = capture(&transaction, capture_id)?;
    let pending: u64 = sql(transaction.query_row(
      "SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?1",
      [capture_id],
      |row| row.get(0),
    ))?;
    if pending != 0
      || current.file_offset != previous.file_offset
      || current.file_identity != previous.file_identity
      || current.end_ms != previous.end_ms
      || current.coverage_lost != previous.coverage_lost
      || current.closed
    {
      return Err(rejection(
        409,
        "run capture changed while recording its end; retry finalization",
      ));
    }
    sql(transaction.execute("UPDATE system_run_captures SET file_offset=?2,closed=1,complete=?3,warnings=?4 WHERE capture_id=?1", params![capture_id, row.file_offset, complete, json(&row.warnings)?]))?;
    sql(transaction.commit())?;
    save_checkpoint(&row)
  }

  /// The independent exporter materializes committed observations. Supervisors
  /// can also finalize terminal captures while this component is paused, and
  /// network failures leave the durable outbox available for local recovery.
  pub fn export_run_captures(&mut self) -> Result<()> {
    let ids = {
      let mut statement = sql(self.connection.prepare("SELECT capture_id FROM system_run_captures WHERE closed=0 OR metadata_saved=0 ORDER BY capture_id"))?;
      sql(statement.query_map([], |row| row.get::<_, String>(0)))?
        .collect::<rusqlite::Result<Vec<_>>>()
    };
    for id in sql(ids)? {
      // An inaccessible run remains in the durable outbox; other runs and the
      // independent machine collector continue normally.
      let _ = (|| -> Result<()> {
        let mut row = capture(&self.connection, &id)?;
        let state_path = row.run_dir.join("run-state.json");
        let state = read_state(&state_path).ok();
        if !row.closed {
          if let Some(state) = &state {
            if state["system_capture_id"] != id {
              return Err(rejection(409, "run capture metadata changed"));
            }
            if row.end_ms.is_none() && terminal(state) {
              self.close_run_capture(&id, time(&state["finished_at"])?)?;
            }
          }
          self.export_run_capture(&id)?;
          row = capture(&self.connection, &id)?;
          let pending: u64 = sql(self.connection.query_row(
            "SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?1",
            [&id],
            |row| row.get(0),
          ))?;
          if row.end_ms.is_some() && row.acknowledged && pending == 0 {
            self.finish_capture(&id, true, Vec::new())?;
            row = capture(&self.connection, &id)?;
          }
        }
        if row.closed {
          // DB close may commit just before a checkpoint filesystem error.
          // Publish terminal capture metadata only after repairing that prefix.
          save_checkpoint(&row)?;
          publish_capture_metadata(&self.connection, &row)?;
        }
        Ok(())
      })();
    }
    Ok(())
  }
}
