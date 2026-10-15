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
