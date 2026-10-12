mod config;
mod daemon;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use expri_client::SystemProfileClient;
use expri_core::lock::{self, LockAttempt};
use expri_core::storage::StoragePaths;
use expri_core::system_profile::{DETAIL_RETENTION_MS, ProfileHistoryQuery, ProfileResolution};
use expri_system::collector::Collector;
use expri_system::store::LocalStore;
use serde::Serialize;
use serde_json::json;

use crate::error::{ExpriError, Result};
use config::GlobalConfig;

#[derive(Debug, Args)]
pub struct SystemCommand {
  /// Override EXPRI_STORAGE_DIR and the default ~/.tokn/expri directory.
  #[arg(long, global = true)]
  storage_dir: Option<PathBuf>,
  #[command(subcommand)]
  command: SystemSubcommand,
}

#[derive(Debug, Subcommand)]
enum SystemSubcommand {
  /// Collect one inventory and sample, saving them in global storage.
  Sample,
  /// Continuously collect locally and independently publish configured batches.
  Daemon {
    /// Override the global configuration; defaults to ten seconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=60))]
    interval_seconds: Option<u64>,
    /// Machine service client configuration; overrides global publishing settings.
    #[arg(long, conflicts_with = "no_publish")]
    client_config: Option<PathBuf>,
    /// Collect locally without publishing this daemon's samples.
    #[arg(long)]
    no_publish: bool,
  },
  /// Inspect local collection and publishing status without creating storage.
  Status,
  /// Read recent details or historical hourly summaries.
  History(HistoryArgs),
  /// Publish one bounded pending batch without starting collection.
  Publish {
    #[arg(long)]
    client_config: Option<PathBuf>,
  },
}

#[derive(Debug, Args)]
struct HistoryArgs {
  /// Inclusive RFC3339 timestamp; defaults to 72h of detail or 30d of hourly data.
  #[arg(long)]
  start: Option<String>,
  /// Exclusive RFC3339 timestamp; defaults to now.
  #[arg(long)]
  end: Option<String>,
  #[arg(long, default_value = "detail", value_parser = ["detail", "hourly"])]
  resolution: String,
  #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1024))]
  limit: u32,
  /// Continue from next_cursor in a prior response.
  #[arg(long)]
  cursor: Option<i64>,
}

impl HistoryArgs {
  fn query(self, machine_id: String) -> Result<ProfileHistoryQuery> {
    let now = now_ms();
    let resolution = if self.resolution == "hourly" {
      ProfileResolution::Hourly
    } else {
      ProfileResolution::Detail
    };
    let span = if resolution == ProfileResolution::Detail {
      DETAIL_RETENTION_MS
    } else {
      30 * 24 * 60 * 60 * 1000
    };
    let end_ms = timestamp(self.end, now.saturating_add(1))?;
    let query = ProfileHistoryQuery {
      machine_id,
      start_ms: timestamp(self.start, end_ms.saturating_sub(span).max(0))?,
      end_ms,
      resolution,
      cursor: self.cursor,
      limit: self.limit as usize,
    };
    query.validate()?;
    Ok(query)
  }
}

fn timestamp(value: Option<String>, default: i64) -> Result<i64> {
  value
    .map(|value| {
      DateTime::parse_from_rfc3339(&value)
        .map(|time| time.timestamp_millis())
        .map_err(|_| ExpriError::Message("system history timestamps must use RFC3339".into()))
    })
    .unwrap_or(Ok(default))
}

pub(super) fn now_ms() -> i64 {
  Utc::now().timestamp_millis()
}

fn print(value: &impl Serialize) -> Result<()> {
  println!("{}", serde_json::to_string_pretty(value)?);
  Ok(())
}

fn writable_paths(paths: &StoragePaths) -> Result<StoragePaths> {
  paths.ensure_private()?;
  StoragePaths::from_root(std::fs::canonicalize(paths.root())?)
}

fn existing_store(paths: &StoragePaths) -> Result<LocalStore> {
  LocalStore::open_readonly(&paths.profile_database())?.ok_or_else(|| {
    ExpriError::Message(
      "no local system profile yet; run expri system sample or expri system daemon".into(),
    )
  })
}

pub fn run(command: SystemCommand, target: Option<&str>) -> Result<()> {
  if target.is_some() {
    return Err(ExpriError::Message(
      "system commands run on the current machine; omit -T".into(),
    ));
  }
  let paths = StoragePaths::resolve(command.storage_dir.as_deref())?;
  paths.validate_existing()?;
  match command.command {
    SystemSubcommand::Sample => {
      let paths = writable_paths(&paths)?;
      let _lease = daemon::collector_lease(&paths)?;
      let mut store = LocalStore::open(&paths.profile_database())?;
      let mut collector = Collector::new()?;
      let (inventory, sample) = collector.collect()?;
      let sample = store.record(inventory, sample)?;
      let _ = store.export_run_captures();
      store.compact(now_ms())?;
      print(
        &json!({"machine_id": store.machine_id(), "sample": sample, "machine": store.profile()?}),
      )
    }
    SystemSubcommand::Daemon {
      interval_seconds,
      client_config,
      no_publish,
    } => {
      let config = GlobalConfig::load(&paths.config_file())?;
      let interval = config.interval(interval_seconds)?;
      let client_config = if no_publish {
        None
      } else {
        config.client_config(client_config)
      };
      daemon::run(writable_paths(&paths)?, interval, client_config)
    }
    SystemSubcommand::Status => status(&paths),
    SystemSubcommand::History(options) => {
      let store = existing_store(&paths)?;
      let query = options.query(store.machine_id().into())?;
      print(&store.history(&query)?)
    }
    SystemSubcommand::Publish { client_config } => {
      let config = GlobalConfig::load(&paths.config_file())?;
      let client_config = config.client_config(client_config).ok_or_else(|| {
        ExpriError::Message(
          "publishing requires --client-config or system_profile.publish.client_config".into(),
        )
      })?;
      let database = paths.profile_database();
      if !database.exists() {
        return Err(ExpriError::Message(
          "no local system profile to publish".into(),
        ));
      }
      let paths = writable_paths(&paths)?;
      let client = SystemProfileClient::new(&client_config)?;
      let mut store = LocalStore::open(&paths.profile_database())?;
      let acknowledgement = daemon::publish_batch(&mut store, &client)?;
      print(&json!({"published": acknowledgement.is_some(), "acknowledgement": acknowledgement}))
    }
  }
}

fn status(paths: &StoragePaths) -> Result<()> {
  let store = LocalStore::open_readonly(&paths.profile_database())?;
  let active = if paths.root().exists() {
    let root = std::fs::canonicalize(paths.root())?;
    matches!(
      lock::try_lock_file_readonly(&root.join("collector.lock"))?,
      LockAttempt::Busy
    )
  } else {
    false
  };
  let publishing = daemon::read_status(&paths.root().join("publishing.json"))?;
  print(&json!({
    "storage_dir": paths.root(), "collector_running": active,
    "local": store.as_ref().map(LocalStore::status).transpose()?,
    "machine": store.as_ref().map(LocalStore::profile).transpose()?.flatten(),
    "publishing": publishing,
  }))
}

#[derive(Debug, Args)]
pub struct ServiceSystemCommand {
  #[command(subcommand)]
  command: ServiceSystemSubcommand,
}

#[derive(Debug, Subcommand)]
enum ServiceSystemSubcommand {
  /// List system-profile machines using an owner service client.
  List {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=100))]
    limit: u32,
    #[arg(long)]
    cursor: Option<String>,
  },
  /// Inspect a current or historical machine inventory.
  Profile {
    #[arg(long)]
    config: PathBuf,
    machine_id: String,
    #[arg(long)]
    inventory_revision: Option<u64>,
  },
  /// Read retained machine details or permanent hourly history.
  History {
    #[arg(long)]
    config: PathBuf,
    machine_id: String,
    #[command(flatten)]
    options: HistoryArgs,
  },
}

pub fn run_service(command: ServiceSystemCommand) -> Result<()> {
  match command.command {
    ServiceSystemSubcommand::List {
      config,
      limit,
      cursor,
    } => {
      let client = SystemProfileClient::new(&config)?;
      let (machines, next_cursor) = client.machines(limit as usize, cursor.as_deref())?;
      print(&json!({"machines": machines, "next_cursor": next_cursor}))
    }
    ServiceSystemSubcommand::Profile {
      config,
      machine_id,
      inventory_revision,
    } => print(&SystemProfileClient::new(&config)?.machine(&machine_id, inventory_revision)?),
    ServiceSystemSubcommand::History {
      config,
      machine_id,
      options,
    } => print(&SystemProfileClient::new(&config)?.history(&options.query(machine_id)?)?),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{Cli, Command};
  use clap::Parser;

  #[test]
  fn system_options_and_service_history_do_not_require_a_project() {
    for args in [
      vec!["expri", "system", "status", "--storage-dir", "/tmp/profile"],
      vec![
        "expri",
        "system",
        "daemon",
        "--interval-seconds",
        "10",
        "--no-publish",
      ],
      vec![
        "expri",
        "service",
        "system",
        "history",
        "--config",
        "owner.toml",
        "machine-1",
        "--resolution",
        "hourly",
      ],
    ] {
      assert!(Cli::try_parse_from(args).is_ok());
    }
    assert!(Cli::try_parse_from(["expri", "system", "daemon", "--interval-seconds", "0"]).is_err());
    let parsed = Cli::try_parse_from(["expri", "-T", "gpu", "system", "status"]).unwrap();
    let Command::System(command) = parsed.command else {
      unreachable!()
    };
    assert!(
      run(command, Some("gpu"))
        .unwrap_err()
        .to_string()
        .contains("current machine")
    );
  }

  #[test]
  fn history_requires_explicit_timezone_and_valid_order() {
    assert!(timestamp(Some("2026-10-12".into()), 0).is_err());
    assert_eq!(
      timestamp(Some("1970-01-01T08:00:01+08:00".into()), 0).unwrap(),
      1000
    );
    let options = HistoryArgs {
      start: Some("2026-10-12T02:00:00Z".into()),
      end: Some("2026-10-12T01:00:00Z".into()),
      resolution: "hourly".into(),
      limit: 1,
      cursor: None,
    };
    assert!(options.query("machine-1".into()).is_err());
  }

  #[test]
  fn a_historical_end_selects_the_preceding_default_window() {
    let options = HistoryArgs {
      start: None,
      end: Some("2026-01-01T00:00:00Z".into()),
      resolution: "hourly".into(),
      limit: 1,
      cursor: None,
    };
    let query = options.query("machine-1".into()).unwrap();
    assert_eq!(query.end_ms - query.start_ms, 30 * 24 * 60 * 60 * 1000);
  }
}
