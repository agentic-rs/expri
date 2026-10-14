mod config;
mod daemon;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use expri_client::{SystemProfileClient, resolve_client_config};
use expri_core::lock::{self, LockAttempt};
use expri_core::storage::StoragePaths;
use expri_core::system_profile::{DETAIL_RETENTION_MS, ProfileHistoryQuery, ProfileResolution};
use expri_system::collector::Collector;
use expri_system::store::LocalStore;
use serde::Serialize;
use serde_json::json;

use crate::error::{ExpriError, Result};
use crate::output::print_report;
use config::{CollectionConfig, GlobalConfig};

#[derive(Debug, Args)]
pub struct SystemCommand {
  /// Print the complete report as JSON instead of a human-readable table.
  #[arg(long, global = true)]
  json: bool,
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
  /// Deprecated compatibility alias for service start --foreground.
  Daemon {
    /// Override the global configuration; defaults to ten seconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=60))]
    interval_seconds: Option<u64>,
    /// Machine service client configuration; overrides global publishing settings.
    #[arg(long, conflicts_with = "no_publish")]
    client_config: Option<PathBuf>,
    /// Collect locally without publishing this service's machine-profile samples.
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

fn print(value: &impl Serialize, json: bool, quiet: bool) -> Result<()> {
  if json || !quiet {
    print_report(value, json)?;
  }
  Ok(())
}

fn writable_paths(paths: &StoragePaths) -> Result<StoragePaths> {
  paths.ensure_private()?;
  StoragePaths::from_root(std::fs::canonicalize(paths.root())?)
}

fn existing_store(paths: &StoragePaths) -> Result<LocalStore> {
  LocalStore::open_readonly(&paths.profile_database())?.ok_or_else(|| {
    ExpriError::Message(
      "no local system profile yet; run expri system sample or expri service start --foreground"
        .into(),
    )
  })
}

pub fn run(command: SystemCommand, target: Option<&str>, quiet: bool) -> Result<()> {
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
        command.json,
        quiet,
      )
    }
    SystemSubcommand::Daemon {
      interval_seconds,
      client_config,
      no_publish,
    } => {
      let config = CollectionConfig::load(&paths, interval_seconds, client_config, no_publish)?;
      let paths = writable_paths(&paths)?;
      eprintln!(
        "expri system daemon is deprecated; use expri service start --foreground. Running the shared worker service."
      );
      let collection_paths = paths.clone();
      expri_client::worker_service::run_foreground(&paths, move |context| {
        collect_with_config(collection_paths, config, context)
      })
    }
    SystemSubcommand::Status => status(&paths, command.json, quiet),
    SystemSubcommand::History(options) => {
      let store = existing_store(&paths)?;
      let query = options.query(store.machine_id().into())?;
      print(&store.history(&query)?, command.json, quiet)
    }
    SystemSubcommand::Publish { client_config } => {
      let client_config = publishing_config(&paths, client_config)?;
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
      print(
        &json!({"published": acknowledgement.is_some(), "acknowledgement": acknowledgement}),
        command.json,
        quiet,
      )
    }
  }
}

/// The worker runtime calls this hook in its collection thread. Local readiness
/// is independent of machine-profile credentials and network availability.
pub(crate) fn collect_for_service(
  paths: StoragePaths,
  context: expri_client::worker_service::ServiceContext,
) -> Result<()> {
  let config = CollectionConfig::load(&paths, None, None, false)?;
  collect_with_config(writable_paths(&paths)?, config, context)
}

fn collect_with_config(
  paths: StoragePaths,
  config: CollectionConfig,
  context: expri_client::worker_service::ServiceContext,
) -> Result<()> {
  // The application composes execution with collection; the client runtime has
  // no dependency on the worker crate. Local collection readiness stays separate.
  let _executor = start_executor(&paths, &context);
  daemon::run(paths, config, context)
}

struct ExecutorLane {
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

impl Drop for ExecutorLane {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

fn configured_executor(
  paths: &StoragePaths,
) -> Result<Option<expri_core::global_config::QueueWorkerConfig>> {
  use expri_core::global_config::{QueueWorkerConfig, read_config_text};

  let Some(text) = read_config_text(&paths.config_file())? else {
    return Ok(None);
  };
  let mut config: toml::Value = toml::from_str(&text)
    .map_err(|_| ExpriError::Message("global expri configuration is invalid".into()))?;
  let Some(worker) = config
    .as_table_mut()
    .and_then(|table| table.remove("queue_worker"))
  else {
    return Ok(None);
  };
  let worker: QueueWorkerConfig = worker
    .try_into()
    .map_err(|_| ExpriError::Message("queue worker configuration is invalid".into()))?;
  worker.validate()?;
  Ok(Some(worker))
}

fn resolve_executor_capacity(
  worker: &expri_core::global_config::QueueWorkerConfig,
  detect: impl FnOnce() -> Result<expri_core::resource_queue::WorkerCapacity>,
) -> Result<expri_core::global_config::ResolvedQueueWorkerConfig> {
  use expri_core::resource_queue::WorkerCapacity;
  let capacity = match (
    worker.capacity.cpus,
    worker.capacity.memory_bytes,
    &worker.capacity.gpus,
  ) {
    (Some(cpus), Some(memory_bytes), Some(gpus)) => WorkerCapacity {
      cpus,
      memory_bytes,
      gpus: gpus.clone(),
    },
    _ => detect()?,
  };
  worker.resolve_capacity(capacity)
}

fn start_executor(
  paths: &StoragePaths,
  context: &expri_client::worker_service::ServiceContext,
) -> Option<ExecutorLane> {
  let worker = match configured_executor(paths) {
    Ok(Some(worker)) => worker,
    Ok(None) => {
      context.execution_status(json!({"status": "disabled", "online": false}));
      return None;
    }
    Err(_) => {
      context.execution_status(json!({"status": "failed", "online": false,
        "last_error": "worker_configuration_invalid"}));
      return None;
    }
  };
  context.execution_status(json!({"status": "detecting", "worker_id": worker.worker_id,
    "online": false, "last_error": null}));
  let stop = context.stop_flag();
  let executor_context = context.clone();
  let executor_stop = stop.clone();
  let execution_paths = paths.clone();
  let task = thread::Builder::new()
    .name("expri-queue-executor".into())
    .spawn(move || {
      let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let worker = resolve_executor_capacity(&worker, expri_system::detect_worker_capacity)?;
        let options = expri_worker::queue::ExecutorOptions {
          storage: execution_paths.clone(),
          worker,
          client_config: execution_paths.config_file(),
          poll_interval: Duration::from_secs(2),
        };
        expri_worker::queue::run_executor(options, executor_stop, |report| {
          executor_context.execution_status(report);
        })
      }));
      if !matches!(outcome, Ok(Ok(()))) {
        executor_context.execution_status(json!({"status": "failed", "online": false,
          "last_error": "execution_unavailable"}));
      }
    });
  match task {
    Ok(thread) => Some(ExecutorLane {
      stop,
      thread: Some(thread),
    }),
    Err(_) => {
      context.execution_status(json!({"status": "failed", "online": false,
        "last_error": "execution_thread_unavailable"}));
      None
    }
  }
}

// An explicit publishing request may use [server]. Continuous collection still
// enables its publisher only through an explicit or saved profile publishing setting.
fn publishing_config(paths: &StoragePaths, explicit: Option<PathBuf>) -> Result<PathBuf> {
  if let Some(explicit) = explicit {
    return resolve_client_config(Some(&explicit));
  }
  let config = GlobalConfig::load(&paths.config_file())?;
  let selected = config
    .client_config(None)
    .unwrap_or_else(|| paths.config_file());
  resolve_client_config(Some(&selected))
}

fn status(paths: &StoragePaths, json: bool, quiet: bool) -> Result<()> {
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
  let collection = daemon::read_status(&paths.root().join("collector.json"))?;
  print(
    &json!({
      "storage_dir": paths.root(), "collector_running": active,
      "local": store.as_ref().map(LocalStore::status).transpose()?,
      "machine": store.as_ref().map(LocalStore::profile).transpose()?.flatten(),
      "publishing": publishing,
      "collection": collection,
    }),
    json,
    quiet,
  )
}

#[derive(Debug, Args)]
pub struct ServiceSystemCommand {
  #[command(subcommand)]
  command: ServiceSystemSubcommand,
}

#[derive(Debug, Subcommand)]
enum ServiceSystemSubcommand {
  /// List system-profile machines visible to the configured server credentials.
  List {
    /// Override the global [server] connection with a standalone client file.
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=100))]
    limit: u32,
    #[arg(long)]
    cursor: Option<String>,
  },
  /// Inspect a current or historical machine inventory.
  Profile {
    /// Override the global [server] connection with a standalone client file.
    #[arg(long)]
    config: Option<PathBuf>,
    machine_id: String,
    #[arg(long)]
    inventory_revision: Option<u64>,
  },
  /// Read retained machine details or permanent hourly history.
  History {
    /// Override the global [server] connection with a standalone client file.
    #[arg(long)]
    config: Option<PathBuf>,
    machine_id: String,
    #[command(flatten)]
    options: HistoryArgs,
  },
}

pub fn run_service(command: ServiceSystemCommand, json: bool, quiet: bool) -> Result<()> {
  match command.command {
    ServiceSystemSubcommand::List {
      config,
      limit,
      cursor,
    } => {
      let client = SystemProfileClient::new(&resolve_client_config(config.as_deref())?)?;
      let (machines, next_cursor) = client.machines(limit as usize, cursor.as_deref())?;
      print(
        &json!({"machines": machines, "next_cursor": next_cursor}),
        json,
        quiet,
      )
    }
    ServiceSystemSubcommand::Profile {
      config,
      machine_id,
      inventory_revision,
    } => print(
      &SystemProfileClient::new(&resolve_client_config(config.as_deref())?)?
        .machine(&machine_id, inventory_revision)?,
      json,
      quiet,
    ),
    ServiceSystemSubcommand::History {
      config,
      machine_id,
      options,
    } => print(
      &SystemProfileClient::new(&resolve_client_config(config.as_deref())?)?
        .history(&options.query(machine_id)?)?,
      json,
      quiet,
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{Cli, Command};
  use clap::Parser;

  #[test]
  fn collection_machines_do_not_execute_server_tasks_without_explicit_enrollment() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().canonicalize().unwrap()).unwrap();
    assert!(configured_executor(&paths).unwrap().is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    std::fs::write(
      paths.config_file(),
      "[server]\nurl='https://server.example.invalid'\ntoken='private-fixture-token'\n",
    )
    .unwrap();
    assert!(configured_executor(&paths).unwrap().is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
  }

  #[test]
  fn executor_enrollment_loads_budget_and_project_allowlist_without_server_contact() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().canonicalize().unwrap()).unwrap();
    std::fs::write(
      paths.config_file(),
      "[queue_worker]\nworker_id='worker-a'\nproject_ids=['vision']\n[queue_worker.capacity]\ncpus=2\nmemory_bytes=8192\n",
    )
    .unwrap();
    let worker = configured_executor(&paths).unwrap().unwrap();
    assert_eq!(worker.worker_id, "worker-a");
    assert_eq!(worker.project_ids, ["vision"]);
    assert_eq!(worker.capacity.cpus, Some(2));
    assert_eq!(worker.capacity.gpus, None);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);

    // Broken executor configuration is isolated from collection readiness.
    std::fs::write(paths.config_file(), "[queue_worker]\nworker_id=42\n").unwrap();
    assert!(configured_executor(&paths).is_err());
    assert!(CollectionConfig::load(&paths, None, None, false).is_ok());
  }

  #[test]
  fn executor_capacity_infers_missing_fields_and_skips_detection_for_explicit_budgets() {
    use expri_core::resource_queue::WorkerCapacity;
    let worker =
      toml::from_str("worker_id='worker-a'\nproject_ids=['vision']\n[capacity]\ncpus=2\ngpus=[]\n")
        .unwrap();
    let resolved = resolve_executor_capacity(&worker, || {
      Ok(WorkerCapacity {
        cpus: 8,
        memory_bytes: 8192,
        gpus: vec![],
      })
    })
    .unwrap();
    assert_eq!(resolved.capacity.cpus, 2);
    assert_eq!(resolved.capacity.memory_bytes, 8192);
    let worker = toml::from_str("worker_id='worker-a'\nproject_ids=['vision']\n[capacity]\ncpus=2\nmemory_bytes=4096\ngpus=[]\n").unwrap();
    let resolved = resolve_executor_capacity(&worker, || {
      panic!("explicit budget must not probe hardware")
    })
    .unwrap();
    assert_eq!(resolved.capacity.memory_bytes, 4096);
  }

  #[test]
  fn report_json_flags_work_before_and_after_nested_commands() {
    for operation in ["sample", "status", "history", "publish"] {
      let default = Cli::try_parse_from(["expri", "system", operation]).unwrap();
      let Command::System(command) = default.command else {
        unreachable!()
      };
      assert!(!command.json);
      for args in [
        vec!["expri", "system", "--json", operation],
        vec!["expri", "system", operation, "--json"],
      ] {
        let parsed = Cli::try_parse_from(args).unwrap();
        let Command::System(command) = parsed.command else {
          unreachable!()
        };
        assert!(command.json);
      }
    }
    for operation in ["list", "profile", "history"] {
      for position in [2, 4] {
        let mut args = vec!["expri", "service", "system", operation];
        if operation != "list" {
          args.push("machine-1");
        }
        args.insert(position, "--json");
        assert!(Cli::try_parse_from(args).is_ok());
      }
    }
  }

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
        "client.toml",
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
      run(command, Some("gpu"), false)
        .unwrap_err()
        .to_string()
        .contains("current machine")
    );
  }

  #[test]
  fn service_system_reads_accept_global_defaults_and_explicit_client_selection() {
    #[derive(clap::Parser)]
    struct SystemCli {
      #[command(flatten)]
      system: ServiceSystemCommand,
    }
    for operation in ["list", "profile", "history"] {
      for explicit in [false, true] {
        let mut args = vec!["expri", operation];
        if operation != "list" {
          args.push("machine-1");
        }
        if explicit {
          args.extend(["--config", "client.toml"]);
        }
        let parsed = SystemCli::try_parse_from(args).unwrap();
        let config = match parsed.system.command {
          ServiceSystemSubcommand::List { config, .. }
          | ServiceSystemSubcommand::Profile { config, .. }
          | ServiceSystemSubcommand::History { config, .. } => config,
        };
        assert_eq!(config, explicit.then(|| PathBuf::from("client.toml")));
      }
    }
  }

  #[test]
  fn requested_profile_publish_uses_selected_global_server_and_legacy_override() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let paths = StoragePaths::from_root(root.clone()).unwrap();
    std::fs::write(
      paths.config_file(),
      "[server]\nurl='https://service.example.invalid'\ntoken='fixture-server-token'\n",
    )
    .unwrap();
    assert_eq!(
      publishing_config(&paths, None).unwrap(),
      paths.config_file()
    );

    let standalone = root.join("machine.toml");
    std::fs::write(
      &standalone,
      "url='https://service.example.invalid'\ntoken_env='EXPRI_MACHINE_TOKEN'\n",
    )
    .unwrap();
    let global = format!(
      "[system_profile.publish]\nclient_config={}\n",
      toml::Value::String(standalone.to_string_lossy().into_owned())
    );
    std::fs::write(paths.config_file(), global).unwrap();
    assert_eq!(publishing_config(&paths, None).unwrap(), standalone);

    // Explicit client selection does not parse broken unrelated global settings.
    std::fs::write(paths.config_file(), "invalid global settings").unwrap();
    assert_eq!(
      publishing_config(&paths, Some(standalone.clone())).unwrap(),
      standalone
    );
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 2);
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
