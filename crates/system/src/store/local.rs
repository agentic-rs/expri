//! Local sequencing, hourly aggregation, and endpoint-specific delivery cursors.

use super::*;
use std::path::Path;
use std::time::Instant;

use expri_core::storage::{StoragePaths, ensure_private_file};
use serde::Deserialize;

const HOUR_METRIC_BYTES: usize = MAX_PROFILE_RECORD_BYTES - 128 * 1024;
const AGGREGATION_WARNING: &str = "Hourly summaries omitted metric contributions because an aggregation limit was reached; raw readings are retained.";

fn add_warning(warnings: &mut Vec<String>) {
  if warnings.iter().any(|value| value == AGGREGATION_WARNING) {
    return;
  }
  if warnings.len() == 32 {
    warnings.pop();
  }
  warnings.push(AGGREGATION_WARNING.into());
}

fn metric_budget(name: &str, kind: MetricKind) -> Result<usize> {
  let widest = MetricAggregate {
    kind,
    sample_count: u64::MAX,
    coverage_ms: u64::MAX,
    sum: 0.0,
    min: 0.0,
    max: 0.0,
    last: 0.0,
    last_at_ms: i64::MAX,
  };
  // Four finite f64 fields reserve28 characters each (zero encodes as3).
  Ok(serde_json::to_vec(name)?.len() + serde_json::to_vec(&widest)?.len() + 4 * (28 - 3) + 2)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalStatus {
  pub machine_id: String,
  pub sample_sequence: u64,
  pub inventory_revision: u64,
  pub detail_count: u64,
  pub hour_count: u64,
  pub first_sample_at_ms: Option<i64>,
  pub last_sample_at_ms: Option<i64>,
  pub quarantined_sample_count: u64,
  pub quarantined_hour_count: u64,
  pub quarantined_inventory_count: u64,
}

pub struct LocalStore {
  pub(super) connection: Connection,
  machine_id: String,
  retention_anchor: (i64, Instant),
}

impl LocalStore {
  /// Inspect an existing store without creating directories, schema, or identity.
  pub fn open_readonly(path: &Path) -> Result<Option<Self>> {
    if let Some(parent) = path.parent()
      && let Ok(metadata) = std::fs::symlink_metadata(parent)
      && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
      return Err(rejection(
        400,
        "profile database directory must be a real directory",
      ));
    }
    match std::fs::symlink_metadata(path) {
      Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
        return Err(rejection(
          400,
          "profile database must be a regular file without symlinks",
        ));
      }
      Ok(_) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(error.into()),
    }
    for suffix in ["-wal", "-shm", "-journal"] {
      let sidecar = path.with_file_name(format!(
        "{}{suffix}",
        path.file_name().unwrap().to_string_lossy()
      ));
      if let Ok(metadata) = std::fs::symlink_metadata(&sidecar)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
      {
        return Err(rejection(
          400,
          "profile database sidecars must be regular files without symlinks",
        ));
      }
    }
    let connection = sql(Connection::open_with_flags(
      path,
      rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ))?;
    sql(connection.busy_timeout(Duration::from_secs(5)))?;
    let machine_id: String = sql(connection.query_row(
      "SELECT machine_id FROM system_local WHERE singleton=1",
      [],
      |row| row.get(0),
    ))?;
    validate_component(&machine_id).map_err(invalid)?;
    Ok(Some(Self {
      connection,
      machine_id,
      retention_anchor: (chrono::Utc::now().timestamp_millis(), Instant::now()),
    }))
  }

  /// Opening is explicitly a write operation; all managed DB files stay private.
  pub fn open(path: &Path) -> Result<Self> {
    Self::open_at(path, chrono::Utc::now().timestamp_millis())
  }

  /// Open known local storage for capture registration without creating an
  /// absent database or silently initializing an unrelated/malformed store.
  pub fn open_existing(path: &Path) -> Result<Option<Self>> {
    let Some(existing) = Self::open_readonly(path)? else {
      return Ok(None);
    };
    sql(
      existing
        .connection
        .prepare("SELECT sequence,inventory_revision FROM system_local WHERE singleton=1"),
    )?;
    sql(
      existing
        .connection
        .prepare("SELECT sequence,observed_at_ms,payload FROM system_samples LIMIT 0"),
    )?;
    sql(
      existing
        .connection
        .prepare("SELECT revision,payload FROM system_inventories LIMIT 0"),
    )?;
    let machine_id = existing.machine_id;
    let connection = sql(Connection::open_with_flags(
      path,
      rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    ))?;
    sql(connection.busy_timeout(Duration::from_secs(5)))?;
    sql(connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;"))?;
    run_capture::initialize(&connection)?;
    let actual: String = sql(connection.query_row(
      "SELECT machine_id FROM system_local WHERE singleton=1",
      [],
      |row| row.get(0),
    ))?;
    if actual != machine_id {
      return Err(rejection(
        409,
        "local machine identity changed while opening capture storage",
      ));
    }
    Ok(Some(Self {
      connection,
      machine_id,
      retention_anchor: (chrono::Utc::now().timestamp_millis(), Instant::now()),
    }))
  }

  /// The startup wall clock is trusted. During this process, monotonic elapsed
  /// time bounds destructive retention even if the wall clock jumps forward.
  pub fn open_at(path: &Path, now_ms: i64) -> Result<Self> {
    if now_ms < 0 {
      return Err(rejection(400, "invalid local profile startup clock"));
    }
    let path = std::path::absolute(path)?;
    let parent = path
      .parent()
      .ok_or_else(|| rejection(400, "profile database requires a parent directory"))?;
    StoragePaths::from_root(parent.to_path_buf())?.ensure_private()?;
    drop(ensure_private_file(&path)?);
    // SQLite must not follow attacker-supplied WAL or journal sidecars.
    for suffix in ["-wal", "-shm", "-journal"] {
      let sidecar = path.with_file_name(format!(
        "{}{suffix}",
        path.file_name().unwrap().to_string_lossy()
      ));
      if let Ok(metadata) = std::fs::symlink_metadata(&sidecar)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
      {
        return Err(rejection(
          400,
          "profile database sidecars must be regular files without symlinks",
        ));
      }
    }
    let mut connection = sql(Connection::open(&path))?;
    initialize(&connection)?;
    run_capture::initialize(&connection)?;
    // DELETE mode avoids persistent globally readable SHM files; rollback journals
    // inherit the database's 0600 mode. Readers and writers use bounded busy waits.
    sql(connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;"))?;
    let transaction = sql(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let machine_id: Option<String> = sql(
      transaction
        .query_row(
          "SELECT machine_id FROM system_local WHERE singleton=1",
          [],
          |row| row.get(0),
        )
        .optional(),
    )?;
    let machine_id = match machine_id {
      Some(id) => {
        validate_component(&id).map_err(invalid)?;
        id
      }
      None => {
        let id = format!("machine-{}", uuid::Uuid::new_v4().simple());
        sql(transaction.execute(
          "INSERT INTO system_local(singleton,machine_id) VALUES(1,?1)",
          [&id],
        ))?;
        sql(transaction.execute("INSERT INTO system_machines(machine_id) VALUES(?1)", [&id]))?;
        id
      }
    };
    quarantine::exclude_oversized(&transaction, &machine_id, now_ms)?;
    sql(transaction.commit())?;
    Ok(Self {
      connection,
      machine_id,
      retention_anchor: (now_ms, Instant::now()),
    })
  }

  pub fn machine_id(&self) -> &str {
    &self.machine_id
  }

  pub fn record(
    &mut self,
    inventory: SystemInventory,
    mut sample: SystemSample,
  ) -> Result<SystemSample> {
    validate_inventory(&inventory).map_err(invalid)?;
    if inventory.observed_at_ms > sample.observed_at_ms.saturating_add(CLOCK_SKEW_MS) {
      return Err(rejection(
        400,
        "inventory observation is ahead of its local sample",
      ));
    }
    if sample.sequence != 0 || sample.inventory_revision != 0 {
      return Err(rejection(
        400,
        "local samples must not assign their own sequence or inventory revision",
      ));
    }
    if sample.observed_at_ms < sample.interval_ms as i64 {
      return Err(rejection(
        400,
        "sample interval begins before the supported clock epoch",
      ));
    }
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate),
    )?;
    run_capture::check_support(&transaction, &self.machine_id)?;
    quarantine::repair_clock(&transaction, &self.machine_id, sample.observed_at_ms)?;
    let (sequence, current_revision): (u64, u64) = sql(transaction.query_row(
      "SELECT sequence,inventory_revision FROM system_local WHERE singleton=1",
      [],
      |row| Ok((row.get(0)?, row.get(1)?)),
    ))?;
    sample.sequence = sequence
      .checked_add(1)
      .ok_or_else(|| rejection(400, "local sample sequence is exhausted"))?;
    let previous = if current_revision > 0 {
      Some(decode::<InventoryRevision>(&sql(transaction.query_row(
        "SELECT payload FROM system_inventories WHERE machine_id=?1 AND revision=?2",
        params![self.machine_id, integer(current_revision)?],
        |row| row.get::<_, String>(0),
      ))?)?)
    } else {
      None
    };
    let revision =
      if !quarantine::inventory_quarantined(&transaction, &self.machine_id, current_revision)?
        && previous
          .as_ref()
          .is_some_and(|previous| equivalent_inventory(&previous.inventory, &inventory))
      {
        current_revision
      } else {
        let revision = current_revision
          .checked_add(1)
          .ok_or_else(|| rejection(400, "local inventory revisions are exhausted"))?;
        put_inventory(
          &transaction,
          &self.machine_id,
          &InventoryRevision {
            revision,
            inventory,
          },
        )?;
        revision
      };
    sample.inventory_revision = revision;
    validate_sample(&sample).map_err(invalid)?;
    if aggregate_sample(&transaction, &self.machine_id, &sample)? > 0 {
      add_warning(&mut sample.warnings);
    }
    validate_sample(&sample).map_err(invalid)?;
    put_sample(
      &transaction,
      &self.machine_id,
      &sample,
      sample.observed_at_ms,
    )?;
    sql(transaction.execute(
      "UPDATE system_local SET sequence=?1,inventory_revision=?2 WHERE singleton=1",
      params![integer(sample.sequence)?, integer(revision)?],
    ))?;
    run_capture::record(&transaction, &self.machine_id, &sample)?;
    sql(transaction.commit())?;
    Ok(sample)
  }

  pub fn profile(&self) -> Result<Option<MachineProfile>> {
    machine_profile(&self.connection, &self.machine_id, None)
  }
  pub fn inventory(&self, revision: Option<u64>) -> Result<Option<MachineProfile>> {
    machine_profile(&self.connection, &self.machine_id, revision)
  }
  pub fn history(&self, query: &ProfileHistoryQuery) -> Result<ProfileHistory> {
    self.history_at(query, chrono::Utc::now().timestamp_millis())
  }
  pub fn history_at(&self, query: &ProfileHistoryQuery, now_ms: i64) -> Result<ProfileHistory> {
    query.validate().map_err(invalid)?;
    let mut query = query.clone();
    if query.resolution == ProfileResolution::Detail {
      query.start_ms = query
        .start_ms
        .max(now_ms.saturating_sub(DETAIL_RETENTION_MS));
      if query.start_ms >= query.end_ms {
        return Ok(ProfileHistory {
          machine_id: query.machine_id,
          resolution: query.resolution,
          samples: Vec::new(),
          hours: Vec::new(),
          next_cursor: None,
        });
      }
    }
    history(&self.connection, &query)
  }
  pub fn compact(&mut self, now_ms: i64) -> Result<usize> {
    if now_ms < 0 {
      return Err(rejection(400, "invalid local profile retention clock"));
    }
    let elapsed: i64 = self
      .retention_anchor
      .1
      .elapsed()
      .as_millis()
      .try_into()
      .unwrap_or(i64::MAX);
    let retention_now = now_ms.min(
      self
        .retention_anchor
        .0
        .saturating_add(elapsed)
        .saturating_add(CLOCK_SKEW_MS),
    );
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate),
    )?;
    run_capture::check_support(&transaction, &self.machine_id)?;
    let supported: bool = sql(transaction.query_row("SELECT EXISTS(SELECT 1 FROM system_run_capture_support p JOIN system_local l ON l.singleton=p.singleton WHERE p.sequence=l.sequence)", [], |row| row.get(0)))?;
    quarantine::repair_clock(&transaction, &self.machine_id, now_ms)?;
    if supported {
      run_capture::update_support(&transaction)?;
    }
    let removed = prune_transaction(&transaction, retention_now, PRUNE_BATCH)?;
    sql(transaction.commit())?;
    Ok(removed)
  }

  pub fn status(&self) -> Result<LocalStatus> {
    let (sample_sequence, inventory_revision) = sql(self.connection.query_row(
      "SELECT sequence,inventory_revision FROM system_local WHERE singleton=1",
      [],
      |row| Ok((row.get(0)?, row.get(1)?)),
    ))?;
    let (detail_count,first_sample_at_ms,last_sample_at_ms) = sql(self.connection.query_row("SELECT COUNT(*),MIN(observed_at_ms),MAX(observed_at_ms) FROM system_samples WHERE machine_id=?1",[&self.machine_id],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))))?;
    let hour_count = sql(self.connection.query_row(
      "SELECT COUNT(*) FROM system_hours WHERE machine_id=?1",
      [&self.machine_id],
      |row| row.get(0),
    ))?;
    let counts = |kind: &str| {
      sql(self.connection.query_row(
        "SELECT COUNT(*) FROM system_local_quarantine WHERE machine_id=?1 AND kind=?2",
        params![self.machine_id, kind],
        |row| row.get(0),
      ))
    };
    Ok(LocalStatus {
      machine_id: self.machine_id.clone(),
      sample_sequence,
      inventory_revision,
      detail_count,
      hour_count,
      first_sample_at_ms,
      last_sample_at_ms,
      quarantined_sample_count: counts("sample")?,
      quarantined_hour_count: counts("hour")?,
      quarantined_inventory_count: counts("inventory")?,
    })
  }

  pub fn pending_batch(&self, endpoint_key: &str) -> Result<ProfileBatch> {
    validate_endpoint(endpoint_key)?;
    let (sample_ack,inventory_ack):(u64,u64) = sql(self.connection.query_row("SELECT sample_sequence,inventory_revision FROM system_endpoint_ack WHERE endpoint_key=?1",[endpoint_key],|row|Ok((row.get(0)?,row.get(1)?))).optional())?.unwrap_or((0,0));
    let mut batch = ProfileBatch {
      schema_version: PROFILE_SCHEMA_VERSION,
      machine_id: self.machine_id.clone(),
      inventories: Vec::new(),
      samples: Vec::new(),
      hours: Vec::new(),
    };
    let header_bytes = serde_json::to_vec(&batch)?.len();
    let mut bytes = header_bytes;
    let mut available = BTreeSet::new();
    let mut statement = sql(self.connection.prepare("SELECT i.payload FROM system_inventories i WHERE i.machine_id=?1 AND revision>?2 AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=i.machine_id AND q.kind='inventory' AND q.record_key=i.revision) ORDER BY revision LIMIT ?3"))?;
    let mut rows = sql(statement.query(params![
      self.machine_id,
      integer(inventory_ack)?,
      MAX_BATCH_INVENTORIES as i64
    ]))?;
    while let Some(row) = sql(rows.next())? {
      let payload: String = sql(row.get(0))?;
      let revision: InventoryRevision = decode(&payload)?;
      if !reserve_payload(&payload, batch.inventories.len(), header_bytes, &mut bytes)? {
        break;
      }
      batch.inventories.push(revision.clone());
      available.insert(revision.revision);
    }
    let can_reference = |revision: u64| revision <= inventory_ack || available.contains(&revision);
    let mut statement = sql(self.connection.prepare("SELECT s.payload FROM system_samples s WHERE s.machine_id=?1 AND sequence>?2 AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=s.machine_id AND q.kind='sample' AND q.record_key=s.sequence) ORDER BY sequence LIMIT ?3"))?;
    let mut rows = sql(statement.query(params![
      self.machine_id,
      integer(sample_ack)?,
      MAX_BATCH_SAMPLES as i64
    ]))?;
    while let Some(row) = sql(rows.next())? {
      let payload: String = sql(row.get(0))?;
      let sample: SystemSample = decode(&payload)?;
      if !can_reference(sample.inventory_revision) {
        break;
      }
      if !reserve_payload(&payload, batch.samples.len(), header_bytes, &mut bytes)? {
        break;
      }
      batch.samples.push(sample);
    }
    let mut statement = sql(self.connection.prepare("SELECT h.payload FROM system_hours h LEFT JOIN system_endpoint_hour_ack a ON a.endpoint_key=?1 AND a.hour_start_ms=h.hour_start_ms WHERE h.machine_id=?2 AND h.revision>COALESCE(a.revision,0) AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=h.machine_id AND q.kind='hour' AND q.record_key=h.hour_start_ms AND q.revision>=h.revision) ORDER BY h.hour_start_ms LIMIT ?3"))?;
    let mut rows = sql(statement.query(params![
      endpoint_key,
      self.machine_id,
      MAX_BATCH_HOURS as i64
    ]))?;
    while let Some(row) = sql(rows.next())? {
      let payload: String = sql(row.get(0))?;
      let hour: SystemHour = decode(&payload)?;
      if !hour
        .inventory_revisions
        .iter()
        .all(|revision| can_reference(*revision))
      {
        break;
      }
      if !reserve_payload(&payload, batch.hours.len(), header_bytes, &mut bytes)? {
        break;
      }
      batch.hours.push(hour);
    }
    Ok(batch)
  }

  /// Commit acknowledgement only after a valid response to this exact batch.
  pub fn acknowledge(
    &mut self,
    endpoint_key: &str,
    batch: &ProfileBatch,
    ack: &ProfileAcknowledgement,
  ) -> Result<()> {
    validate_endpoint(endpoint_key)?;
    validate_profile_batch(batch).map_err(invalid)?;
    if batch.machine_id != self.machine_id || ack.machine_id != self.machine_id {
      return Err(rejection(
        400,
        "system acknowledgement belongs to another machine",
      ));
    }
    let sample_sequence = batch
      .samples
      .iter()
      .map(|sample| sample.sequence)
      .max()
      .unwrap_or(0);
    let inventory_revision = batch
      .inventories
      .iter()
      .map(|inventory| inventory.revision)
      .max()
      .unwrap_or(0);
    let hour_revisions: BTreeMap<_, _> = batch
      .hours
      .iter()
      .map(|hour| (hour.hour_start_ms, hour.revision))
      .collect();
    if ack.sample_sequence != sample_sequence
      || ack.inventory_revision != inventory_revision
      || ack.hour_revisions != hour_revisions
    {
      return Err(rejection(
        400,
        "system acknowledgement does not match the transmitted batch",
      ));
    }
    let transaction = sql(
      self
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate),
    )?;
    sql(transaction.execute("INSERT INTO system_endpoint_ack(endpoint_key,sample_sequence,inventory_revision) VALUES(?1,?2,?3) ON CONFLICT(endpoint_key) DO UPDATE SET sample_sequence=MAX(sample_sequence,excluded.sample_sequence),inventory_revision=MAX(inventory_revision,excluded.inventory_revision)",params![endpoint_key,integer(sample_sequence)?,integer(inventory_revision)?]))?;
    for (hour, revision) in &hour_revisions {
      sql(transaction.execute("INSERT INTO system_endpoint_hour_ack(endpoint_key,hour_start_ms,revision) VALUES(?1,?2,?3) ON CONFLICT(endpoint_key,hour_start_ms) DO UPDATE SET revision=MAX(revision,excluded.revision)",params![endpoint_key,hour,integer(*revision)?]))?;
    }
    sql(transaction.commit())
  }
}

fn validate_endpoint(key: &str) -> Result<()> {
  if key.is_empty() || key.len() > 1024 || key.chars().any(char::is_control) {
    return Err(rejection(400, "invalid profile publisher endpoint key"));
  }
  Ok(())
}
fn reserve_payload(
  payload: &str,
  count: usize,
  header_bytes: usize,
  bytes: &mut usize,
) -> Result<bool> {
  if header_bytes + payload.len() > MAX_PROFILE_BATCH_BYTES {
    return Err(rejection(
      413,
      "one system profile record exceeds the publishing batch size limit",
    ));
  }
  let next = *bytes + payload.len() + usize::from(count > 0);
  if next > MAX_PROFILE_BATCH_BYTES {
    return Ok(false);
  }
  *bytes = next;
  Ok(true)
}

fn equivalent_inventory(left: &SystemInventory, right: &SystemInventory) -> bool {
  let normalize = |inventory: &SystemInventory| {
    let mut inventory = inventory.clone();
    inventory.observed_at_ms = 0;
    inventory
      .devices
      .sort_by(|left, right| left.device_id.cmp(&right.device_id));
    inventory.warnings.sort();
    inventory.warnings.dedup();
    inventory
  };
  normalize(left) == normalize(right)
}

pub(super) fn aggregate_sample(
  transaction: &Transaction<'_>,
  machine_id: &str,
  sample: &SystemSample,
) -> Result<usize> {
  aggregate_sample_for_hour(transaction, machine_id, sample, None)
}

pub(super) fn aggregate_sample_for_hour(
  transaction: &Transaction<'_>,
  machine_id: &str,
  sample: &SystemSample,
  only_hour: Option<i64>,
) -> Result<usize> {
  let start = sample.observed_at_ms - sample.interval_ms as i64;
  let mut slices = Vec::new();
  if sample.interval_ms == 0 {
    slices.push((sample.observed_at_ms / HOUR_MS * HOUR_MS, 0));
  } else {
    let mut cursor = start;
    while cursor < sample.observed_at_ms {
      let hour = cursor / HOUR_MS * HOUR_MS;
      let end = hour.saturating_add(HOUR_MS).min(sample.observed_at_ms);
      slices.push((hour, (end - cursor) as u64));
      cursor = end;
    }
  }
  let mut omitted = 0;
  for (hour_start_ms, coverage_ms) in slices {
    if only_hour.is_some_and(|hour| hour != hour_start_ms) {
      continue;
    }
    let previous: Option<String> = sql(
      transaction
        .query_row(
          "SELECT payload FROM system_hours WHERE machine_id=?1 AND hour_start_ms=?2",
          params![machine_id, hour_start_ms],
          |row| row.get(0),
        )
        .optional(),
    )?;
    let mut hour = match previous {
      Some(payload) => decode::<SystemHour>(&payload)?,
      None => SystemHour {
        hour_start_ms,
        revision: sample.sequence,
        sample_count: 0,
        first_sample_at_ms: sample.observed_at_ms,
        last_sample_at_ms: sample.observed_at_ms,
        inventory_revisions: Vec::new(),
        metrics: BTreeMap::new(),
        warnings: Vec::new(),
      },
    };
    hour.revision = sample.sequence;
    hour.sample_count += 1;
    hour.first_sample_at_ms = hour.first_sample_at_ms.min(sample.observed_at_ms);
    hour.last_sample_at_ms = hour.last_sample_at_ms.max(sample.observed_at_ms);
    if !hour
      .inventory_revisions
      .contains(&sample.inventory_revision)
    {
      hour.inventory_revisions.push(sample.inventory_revision);
      hour.inventory_revisions.sort_unstable();
    }
    let mut budget = 2;
    for (name, aggregate) in &hour.metrics {
      budget += metric_budget(name, aggregate.kind)?;
    }
    let legacy = budget > HOUR_METRIC_BYTES;
    let mut actual_bytes = serde_json::to_vec(&hour)?.len();
    let mut hour_omissions = 0;
    for (name, reading) in &sample.metrics {
      let previous = hour.metrics.get(name);
      let entry_budget = metric_budget(name, reading.kind)?;
      if previous.is_none()
        && (hour.metrics.len() == MAX_METRICS || budget + entry_budget > HOUR_METRIC_BYTES)
      {
        hour_omissions += 1;
        continue;
      }
      let mut aggregate = previous.cloned().unwrap_or(MetricAggregate {
        kind: reading.kind,
        sample_count: 0,
        coverage_ms: 0,
        sum: 0.0,
        min: reading.value,
        max: reading.value,
        last: reading.value,
        last_at_ms: sample.observed_at_ms,
      });
      if aggregate.kind != reading.kind {
        return Err(rejection(400, "system metric changed kind within an hour"));
      }
      aggregate.sample_count += 1;
      aggregate.coverage_ms = aggregate
        .coverage_ms
        .checked_add(coverage_ms)
        .ok_or_else(|| rejection(400, "system aggregate coverage overflow"))?;
      aggregate.sum += match reading.kind {
        MetricKind::Gauge => reading.value * coverage_ms as f64,
        MetricKind::CounterDelta => {
          if sample.interval_ms == 0 {
            reading.value
          } else {
            reading.value * coverage_ms as f64 / sample.interval_ms as f64
          }
        }
      };
      if !aggregate.sum.is_finite() {
        hour_omissions += 1;
        continue;
      }
      aggregate.min = aggregate.min.min(reading.value);
      aggregate.max = aggregate.max.max(reading.value);
      if sample.observed_at_ms >= aggregate.last_at_ms {
        aggregate.last = reading.value;
        aggregate.last_at_ms = sample.observed_at_ms;
      }
      let previous_bytes = previous
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |value| value.len());
      let next_bytes = serde_json::to_vec(&aggregate)?.len();
      let growth = if previous.is_some() {
        next_bytes.saturating_sub(previous_bytes)
      } else {
        serde_json::to_vec(name)?.len() + next_bytes + 2
      };
      // Retained pre-admission snapshots keep their keys. If an update cannot fit
      // the wire limit, omit that contribution explicitly instead of corrupting
      // the outbox or aborting collection.
      if legacy && actual_bytes + growth + 1024 > MAX_PROFILE_RECORD_BYTES {
        hour_omissions += 1;
        continue;
      }
      actual_bytes =
        actual_bytes.saturating_sub(previous_bytes.saturating_sub(next_bytes)) + growth;
      if previous.is_none() {
        budget += entry_budget;
      }
      hour.metrics.insert(name.clone(), aggregate);
    }
    if hour_omissions > 0 {
      add_warning(&mut hour.warnings);
    }
    omitted += hour_omissions;
    validate_hour(&hour).map_err(invalid)?;
    put_hour(transaction, machine_id, &hour)?;
  }
  Ok(omitted)
}
