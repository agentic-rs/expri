//! Invalid local clocks and legacy oversized records are explicit, durable exclusions.
//! Raw quarantine keeps identity/reason only; hourly snapshots remain inspectable data.

use super::*;

pub(super) fn inventory_quarantined(
  connection: &Connection,
  machine_id: &str,
  revision: u64,
) -> Result<bool> {
  sql(connection.query_row("SELECT EXISTS(SELECT 1 FROM system_local_quarantine WHERE machine_id=?1 AND kind='inventory' AND record_key=?2)",params![machine_id,integer(revision)?],|row|row.get(0)))
}

struct QuarantinedRecord<'a> {
  kind: &'a str,
  key: i64,
  revision: u64,
  reason: &'a str,
  payload: Option<&'a str>,
}

fn exclude(
  transaction: &Transaction<'_>,
  machine_id: &str,
  now_ms: i64,
  record: QuarantinedRecord<'_>,
) -> Result<()> {
  sql(transaction.execute("INSERT OR IGNORE INTO system_local_quarantine(machine_id,kind,record_key,revision,reason,quarantined_at_ms,payload) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![machine_id,record.kind,record.key,integer(record.revision)?,record.reason,now_ms,record.payload]))?;
  Ok(())
}

pub(super) fn exclude_oversized(
  transaction: &Transaction<'_>,
  machine_id: &str,
  now_ms: i64,
) -> Result<()> {
  for (table, kind, key, revision) in [
    ("system_samples", "sample", "sequence", "sequence"),
    ("system_inventories", "inventory", "revision", "revision"),
    ("system_hours", "hour", "hour_start_ms", "revision"),
  ] {
    let payload = if kind == "hour" { "payload" } else { "NULL" };
    let encoded_size = if kind == "inventory" {
      "LENGTH(CAST(json_extract(payload,'$.inventory') AS BLOB))"
    } else {
      "LENGTH(CAST(payload AS BLOB))"
    };
    let statement = format!(
      "INSERT OR IGNORE INTO system_local_quarantine(machine_id,kind,record_key,revision,reason,quarantined_at_ms,payload) SELECT machine_id,'{kind}',{key},{revision},'oversized_record',?2,{payload} FROM {table} WHERE machine_id=?1 AND {encoded_size}>?3"
    );
    sql(transaction.execute(
      &statement,
      params![machine_id, now_ms, MAX_PROFILE_RECORD_BYTES as i64],
    ))?;
  }
  // There is no reason to retain quarantined detailed payloads beyond metadata.
  sql(transaction.execute("DELETE FROM system_samples WHERE machine_id=?1 AND EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=system_samples.machine_id AND q.kind='sample' AND q.record_key=system_samples.sequence)",[machine_id]))?;
  Ok(())
}

fn next_sequence(transaction: &Transaction<'_>) -> Result<u64> {
  let sequence: u64 = sql(transaction.query_row(
    "SELECT sequence FROM system_local WHERE singleton=1",
    [],
    |row| row.get(0),
  ))?;
  let next = sequence
    .checked_add(1)
    .ok_or_else(|| rejection(400, "local profile sequence is exhausted"))?;
  sql(transaction.execute(
    "UPDATE system_local SET sequence=?1 WHERE singleton=1",
    [integer(next)?],
  ))?;
  Ok(next)
}

fn replace_inventory_references(
  transaction: &Transaction<'_>,
  machine_id: &str,
  old: u64,
  new: u64,
  now_ms: i64,
) -> Result<()> {
  let mut statement = sql(transaction.prepare(
    "SELECT sequence,payload FROM system_samples WHERE machine_id=?1 AND inventory_revision=?2",
  ))?;
  let rows = sql(
    statement.query_map(params![machine_id, integer(old)?], |row| {
      Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
    }),
  )?
  .collect::<rusqlite::Result<Vec<_>>>();
  for (sequence, payload) in sql(rows)? {
    let mut sample: SystemSample = decode(&payload)?;
    exclude(
      transaction,
      machine_id,
      now_ms,
      QuarantinedRecord {
        kind: "sample",
        key: integer(sequence)?,
        revision: sequence,
        reason: "corrected_inventory",
        payload: None,
      },
    )?;
    sql(transaction.execute(
      "DELETE FROM system_samples WHERE machine_id=?1 AND sequence=?2",
      params![machine_id, integer(sequence)?],
    ))?;
    sample.sequence = next_sequence(transaction)?;
    sample.inventory_revision = new;
    validate_sample(&sample).map_err(invalid)?;
    put_sample(transaction, machine_id, &sample, now_ms)?;
  }
  let mut statement=sql(transaction.prepare("SELECT hour_start_ms FROM system_hours h WHERE machine_id=?1 AND EXISTS(SELECT 1 FROM json_each(h.payload,'$.inventory_revisions') r WHERE r.value=?2)"))?;
  let hours = sql(
    statement.query_map(params![machine_id, integer(old)?], |row| {
      row.get::<_, i64>(0)
    }),
  )?
  .collect::<rusqlite::Result<Vec<_>>>();
  for start in sql(hours)? {
    let payload: String = sql(transaction.query_row(
      "SELECT payload FROM system_hours WHERE machine_id=?1 AND hour_start_ms=?2",
      params![machine_id, start],
      |row| row.get(0),
    ))?;
    let mut hour: SystemHour = decode(&payload)?;
    for revision in &mut hour.inventory_revisions {
      if *revision == old {
        *revision = new;
      }
    }
    hour.inventory_revisions.sort_unstable();
    hour.inventory_revisions.dedup();
    hour.revision = next_sequence(transaction)?;
    validate_hour(&hour).map_err(invalid)?;
    put_hour(transaction, machine_id, &hour)?;
  }
  Ok(())
}

fn repair_inventories(transaction: &Transaction<'_>, machine_id: &str, now_ms: i64) -> Result<()> {
  // Repair only a current inventory or a revision used by retained good data.
  // Original revisions stay immutable and permanently excluded from publishing.
  let mut statement=sql(transaction.prepare("SELECT i.revision,i.payload FROM system_inventories i JOIN system_local_quarantine q ON q.machine_id=i.machine_id AND q.kind='inventory' AND q.record_key=i.revision WHERE i.machine_id=?1 AND q.reason='future_clock' AND (i.revision=(SELECT inventory_revision FROM system_local WHERE singleton=1) OR EXISTS(SELECT 1 FROM system_samples s WHERE s.machine_id=i.machine_id AND s.inventory_revision=i.revision) OR EXISTS(SELECT 1 FROM system_hours h,json_each(h.payload,'$.inventory_revisions') r WHERE h.machine_id=i.machine_id AND r.value=i.revision)) ORDER BY i.revision"))?;
  let rows = sql(statement.query_map([machine_id], |row| {
    Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
  }))?
  .collect::<rusqlite::Result<Vec<_>>>();
  for (old, payload) in sql(rows)? {
    let mut inventory: InventoryRevision = decode(&payload)?;
    inventory.inventory.observed_at_ms = now_ms;
    let latest: u64 = sql(transaction.query_row(
      "SELECT inventory_revision FROM system_local WHERE singleton=1",
      [],
      |row| row.get(0),
    ))?;
    inventory.revision = latest
      .checked_add(1)
      .ok_or_else(|| rejection(400, "local inventory revisions are exhausted"))?;
    validate_inventory(&inventory.inventory).map_err(invalid)?;
    put_inventory(transaction, machine_id, &inventory)?;
    sql(transaction.execute(
      "UPDATE system_local SET inventory_revision=?1 WHERE singleton=1",
      [integer(inventory.revision)?],
    ))?;
    replace_inventory_references(transaction, machine_id, old, inventory.revision, now_ms)?;
  }
  Ok(())
}

pub(super) fn repair_clock(
  transaction: &Transaction<'_>,
  machine_id: &str,
  now_ms: i64,
) -> Result<()> {
  let latest = now_ms.saturating_add(CLOCK_SKEW_MS);
  let latest_hour = latest / HOUR_MS * HOUR_MS;
  // Normal ticks touch only current metadata, not permanent hourly history.
  let needs_repair:bool=sql(transaction.query_row("SELECT COALESCE(m.last_sample_at_ms,0)>?2 OR COALESCE(json_extract(i.payload,'$.inventory.observed_at_ms'),0)>?2 FROM system_machines m JOIN system_local l ON l.machine_id=m.machine_id LEFT JOIN system_inventories i ON i.machine_id=l.machine_id AND i.revision=l.inventory_revision WHERE m.machine_id=?1",params![machine_id,latest],|row|row.get(0)))?;
  if !needs_repair {
    return Ok(());
  }
  sql(transaction.execute("INSERT OR IGNORE INTO system_local_quarantine(machine_id,kind,record_key,revision,reason,quarantined_at_ms,payload) SELECT machine_id,'sample',sequence,sequence,'future_clock',?2,NULL FROM system_samples WHERE machine_id=?1 AND observed_at_ms>?3",params![machine_id,now_ms,latest]))?;
  sql(transaction.execute(
    "DELETE FROM system_samples WHERE machine_id=?1 AND observed_at_ms>?2",
    params![machine_id, latest],
  ))?;
  sql(transaction.execute("INSERT OR IGNORE INTO system_local_quarantine(machine_id,kind,record_key,revision,reason,quarantined_at_ms,payload) SELECT machine_id,'inventory',revision,revision,'future_clock',?2,NULL FROM system_inventories WHERE machine_id=?1 AND json_extract(payload,'$.inventory.observed_at_ms')>?3",params![machine_id,now_ms,latest]))?;
  let mut statement=sql(transaction.prepare("SELECT hour_start_ms,revision,payload FROM system_hours WHERE machine_id=?1 AND hour_start_ms>=?4 AND (hour_start_ms>?2 OR json_extract(payload,'$.last_sample_at_ms')>?3)"))?;
  let rows = sql(statement.query_map(
    params![machine_id, latest_hour, latest, now_ms / HOUR_MS * HOUR_MS],
    |row| {
      Ok((
        row.get::<_, i64>(0)?,
        row.get::<_, u64>(1)?,
        row.get::<_, String>(2)?,
      ))
    },
  ))?
  .collect::<rusqlite::Result<Vec<_>>>();
  let mut rebuild = Vec::new();
  for (start, revision, payload) in sql(rows)? {
    exclude(
      transaction,
      machine_id,
      now_ms,
      QuarantinedRecord {
        kind: "hour",
        key: start,
        revision,
        reason: "future_clock",
        payload: Some(&payload),
      },
    )?;
    sql(transaction.execute(
      "DELETE FROM system_hours WHERE machine_id=?1 AND hour_start_ms=?2",
      params![machine_id, start],
    ))?;
    if start <= latest_hour {
      rebuild.push(start);
    }
  }
  repair_inventories(transaction, machine_id, now_ms)?;
  for start in rebuild {
    let mut statement=sql(transaction.prepare("SELECT payload FROM system_samples WHERE machine_id=?1 AND observed_at_ms>=?2 AND observed_at_ms<=?3 ORDER BY sequence"))?;
    let mut rows = sql(statement.query(params![
        machine_id,
        start,
        start
          .saturating_add(HOUR_MS)
          .saturating_add(MAX_INTERVAL_MS as i64)
      ]))?;
    while let Some(row) = sql(rows.next())? {
      let sample: SystemSample = decode(&sql(row.get::<_, String>(0))?)?;
      local::aggregate_sample_for_hour(transaction, machine_id, &sample, Some(start))?;
    }
    let payload: Option<String> = sql(
      transaction
        .query_row(
          "SELECT payload FROM system_hours WHERE machine_id=?1 AND hour_start_ms=?2",
          params![machine_id, start],
          |row| row.get(0),
        )
        .optional(),
    )?;
    if let Some(payload) = payload {
      let mut hour: SystemHour = decode(&payload)?;
      hour.revision = next_sequence(transaction)?;
      validate_hour(&hour).map_err(invalid)?;
      put_hour(transaction, machine_id, &hour)?;
    }
  }
  // Future observations must not keep the machine's latest timestamp pinned.
  sql(transaction.execute("UPDATE system_machines SET inventory_revision=COALESCE((SELECT MAX(i.revision) FROM system_inventories i WHERE i.machine_id=?1 AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=i.machine_id AND q.kind='inventory' AND q.record_key=i.revision)),0),last_sample_at_ms=(SELECT MAX(t) FROM (SELECT MAX(observed_at_ms) t FROM system_samples WHERE machine_id=?1 UNION ALL SELECT MAX(json_extract(payload,'$.last_sample_at_ms')) t FROM system_hours WHERE machine_id=?1)) WHERE machine_id=?1",[machine_id]))?;
  Ok(())
}
