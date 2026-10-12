//! Observable Linux interface counters. A failed read is absent, never a fabricated zero.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Component, Path};

use super::Warnings;

const COUNTERS: [(&str, &str); 6] = [
  ("received_bytes", "rx_bytes"),
  ("transmitted_bytes", "tx_bytes"),
  ("received_packets", "rx_packets"),
  ("transmitted_packets", "tx_packets"),
  ("receive_errors", "rx_errors"),
  ("transmit_errors", "tx_errors"),
];

pub(super) fn read(name: &str, warnings: &mut Warnings) -> BTreeMap<&'static str, u64> {
  read_at(Path::new("/sys/class/net"), name, warnings)
}

fn read_at(root: &Path, name: &str, warnings: &mut Warnings) -> BTreeMap<&'static str, u64> {
  let mut readings = BTreeMap::new();
  let mut components = Path::new(name).components();
  if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
    warnings.push("A network interface has an invalid sysfs name; its counters were omitted.");
    return readings;
  }
  for (metric, filename) in COUNTERS {
    match number(&root.join(name).join("statistics").join(filename)) {
      Ok(value) => {
        readings.insert(metric, value);
      }
      Err(_) => warnings.push(format!(
        "Network counter {name}/{filename} is unavailable; its interval delta was omitted."
      )),
    }
  }
  readings
}

fn number(path: &Path) -> io::Result<u64> {
  let mut text = String::new();
  std::fs::File::open(path)?
    .take(65)
    .read_to_string(&mut text)?;
  if text.len() > 64 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "network counter exceeds its limit",
    ));
  }
  text
    .trim()
    .parse()
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid network counter"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::collector::CounterTracker;

  #[test]
  fn network_read_failure_drops_only_its_baseline_and_cannot_create_a_rebound_spike() {
    let root = tempfile::tempdir().unwrap();
    let stats = root.path().join("eth0/statistics");
    std::fs::create_dir_all(&stats).unwrap();
    for (_, filename) in COUNTERS {
      std::fs::write(stats.join(filename), "10000\n").unwrap();
    }
    let mut tracker = CounterTracker::default();
    let mut warnings = Warnings::default();
    let read = |warnings: &mut Warnings| -> BTreeMap<String, u64> {
      read_at(root.path(), "eth0", warnings)
        .into_iter()
        .map(|(metric, value)| (format!("network/fixture/{metric}"), value))
        .collect()
    };
    tracker.observe(&read(&mut warnings), false, &mut warnings);
    std::fs::remove_file(stats.join("rx_bytes")).unwrap();
    let failed = read(&mut warnings);
    assert!(!failed.contains_key("network/fixture/received_bytes"));
    let delta = tracker.observe(&failed, true, &mut warnings);
    assert!(!delta.contains_key("network/fixture/received_bytes"));
    assert_eq!(delta["network/fixture/transmitted_bytes"].value, 0.0);
    std::fs::write(stats.join("rx_bytes"), "1000000\n").unwrap();
    assert!(
      !tracker
        .observe(&read(&mut warnings), true, &mut warnings)
        .contains_key("network/fixture/received_bytes")
    );
    std::fs::write(stats.join("rx_bytes"), "1000042\n").unwrap();
    assert_eq!(
      tracker.observe(&read(&mut warnings), true, &mut warnings)["network/fixture/received_bytes"]
        .value,
      42.0
    );
    assert!(!warnings.into_vec().is_empty());
  }

  #[test]
  fn counters_preserve_u64_values_and_omit_malformed_or_oversized_files() {
    let root = tempfile::tempdir().unwrap();
    let stats = root.path().join("eth0/statistics");
    std::fs::create_dir_all(&stats).unwrap();
    std::fs::write(stats.join("rx_bytes"), u64::MAX.to_string()).unwrap();
    std::fs::write(stats.join("tx_bytes"), "invalid").unwrap();
    std::fs::write(stats.join("rx_packets"), "1".repeat(65)).unwrap();
    let mut warnings = Warnings::default();
    let values = read_at(root.path(), "eth0", &mut warnings);
    assert_eq!(values["received_bytes"], u64::MAX);
    assert_eq!(values.len(), 1);
    assert!(read_at(root.path(), "../eth0", &mut warnings).is_empty());
  }
}
