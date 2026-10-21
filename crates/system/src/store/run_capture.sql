BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS system_run_captures (
  capture_id TEXT PRIMARY KEY,
  machine_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  run_dir TEXT NOT NULL,
  start_ms INTEGER NOT NULL,
  end_ms INTEGER,
  acknowledged INTEGER NOT NULL DEFAULT 0,
  coverage_lost INTEGER NOT NULL DEFAULT 0,
  closed INTEGER NOT NULL DEFAULT 0,
  complete INTEGER NOT NULL DEFAULT 0,
  file_offset INTEGER NOT NULL,
  segment_offset INTEGER NOT NULL DEFAULT 0,
  header_pending INTEGER NOT NULL DEFAULT 0,
  file_identity TEXT NOT NULL,
  inventory_revision INTEGER NOT NULL DEFAULT 0,
  exported_sequence INTEGER NOT NULL DEFAULT 0,
  metadata_saved INTEGER NOT NULL DEFAULT 0,
  warnings TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS system_run_capture_pending ON system_run_captures(closed,metadata_saved);
CREATE TABLE IF NOT EXISTS system_run_capture_samples (
  capture_id TEXT NOT NULL,
  sequence INTEGER NOT NULL,
  observed_at_ms INTEGER NOT NULL,
  inventory TEXT NOT NULL,
  sample TEXT NOT NULL,
  PRIMARY KEY(capture_id, sequence)
);
CREATE TABLE IF NOT EXISTS system_run_capture_support (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  sequence INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS system_run_capture_minutes (
  capture_id TEXT NOT NULL,
  minute INTEGER NOT NULL,
  sequence INTEGER NOT NULL,
  PRIMARY KEY(capture_id, minute)
);
CREATE TABLE IF NOT EXISTS system_run_capture_minute_seed (
  capture_id TEXT PRIMARY KEY,
  file_offset INTEGER NOT NULL,
  file_identity TEXT NOT NULL,
  scan_offset INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS system_run_capture_policy (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  version INTEGER NOT NULL
);
-- Snapshot the old committed prefix and reserve pending observations before an
-- old exporter can remove them. New captures require no historical scan.
CREATE TRIGGER IF NOT EXISTS system_run_capture_policy_seed
AFTER INSERT ON system_run_capture_policy
BEGIN
  INSERT OR IGNORE INTO system_run_capture_minute_seed(capture_id,file_offset,file_identity)
    SELECT capture_id,file_offset,file_identity FROM system_run_captures WHERE closed=0;
  INSERT OR IGNORE INTO system_run_capture_minutes(capture_id,minute,sequence)
    SELECT capture_id,
      observed_at_ms/60000-CASE WHEN observed_at_ms%60000<0 THEN 1 ELSE 0 END,
      MIN(sequence)
    FROM system_run_capture_samples
    GROUP BY capture_id,observed_at_ms/60000-CASE WHEN observed_at_ms%60000<0 THEN 1 ELSE 0 END;
END;
-- 60000 matches RUN_SYSTEM_SAMPLE_INTERVAL_MS. Keep the policy in SQLite so
-- already-running collectors obey it as well, with floor division for negatives.
-- Legacy prefixes and pending rows remain unchanged during their one-time seed.
CREATE TRIGGER IF NOT EXISTS system_run_capture_minute_gate
BEFORE INSERT ON system_run_capture_samples
WHEN NOT EXISTS(SELECT 1 FROM system_run_capture_minute_seed WHERE capture_id=NEW.capture_id)
  AND EXISTS(SELECT 1 FROM system_run_capture_minutes
    WHERE capture_id=NEW.capture_id
      AND minute=NEW.observed_at_ms/60000-CASE WHEN NEW.observed_at_ms%60000<0 THEN 1 ELSE 0 END
      AND sequence!=NEW.sequence)
BEGIN
  SELECT RAISE(IGNORE);
END;
CREATE TRIGGER IF NOT EXISTS system_run_capture_minute_reserve
AFTER INSERT ON system_run_capture_samples
BEGIN
  INSERT OR IGNORE INTO system_run_capture_minutes(capture_id,minute,sequence)
    VALUES(NEW.capture_id,
      NEW.observed_at_ms/60000-CASE WHEN NEW.observed_at_ms%60000<0 THEN 1 ELSE 0 END,
      NEW.sequence);
END;
INSERT OR IGNORE INTO system_run_capture_policy(singleton,version) VALUES(1,1);
COMMIT;
