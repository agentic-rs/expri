//! Linux block-device counters are observable reads, unlike sysinfo's best-effort disk I/O.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;

use super::Warnings;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DiskIo {
  pub read_bytes: u64,
  pub written_bytes: u64,
}

pub(super) fn read(warnings: &mut Warnings) -> Option<BTreeMap<String, DiskIo>> {
  if !cfg!(target_os = "linux") {
    return None;
  }
  let result = (|| {
    let mut raw = String::new();
    std::fs::File::open("/proc/diskstats")?
      .take(1024 * 1024 + 1)
      .read_to_string(&mut raw)?;
    if raw.len() > 1024 * 1024 {
      return Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "diskstats exceeds its bound",
      ));
    }
    parse(&raw).ok_or_else(|| {
      std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "diskstats has malformed rows",
      )
    })
  })();
  match result {
    Ok(values) => Some(values),
    Err(_) => {
      warnings.push(
        "Linux disk I/O counters are unavailable; /proc/diskstats could not be read or parsed.",
      );
      None
    }
  }
}

pub(super) fn for_disk(values: &BTreeMap<String, DiskIo>, name: &OsStr) -> Option<DiskIo> {
  let path = Path::new(name);
  if !path.starts_with("/dev") {
    return None;
  }
  let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
  values.get(resolved.file_name()?.to_str()?).copied()
}

fn parse(raw: &str) -> Option<BTreeMap<String, DiskIo>> {
  let mut values = BTreeMap::new();
  for line in raw.lines().filter(|line| !line.trim().is_empty()) {
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() < 14 {
      return None;
    }
    fields[0].parse::<u32>().ok()?;
    fields[1].parse::<u32>().ok()?;
    // Linux reports sectors in fixed 512-byte units, regardless of device sector size.
    let read_bytes = fields[5].parse::<u64>().ok()?.checked_mul(512)?;
    let written_bytes = fields[9].parse::<u64>().ok()?.checked_mul(512)?;
    values.insert(
      fields[2].into(),
      DiskIo {
        read_bytes,
        written_bytes,
      },
    );
  }
  Some(values)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn diskstats_uses_fixed_sector_units_and_keeps_partition_names() {
    let values = parse("8 1 sda1 10 0 8192 2 20 0 4096 3 0 5 6\n").unwrap();
    assert_eq!(
      values["sda1"],
      DiskIo {
        read_bytes: 4_194_304,
        written_bytes: 2_097_152
      }
    );
  }

  #[test]
  fn malformed_or_overflowed_counters_are_unavailable() {
    assert!(parse("8 1 sda1 unknown\n").is_none());
    assert!(parse(&format!("8 1 sda1 0 0 {} 0 0 0 0 0 0 0 0", u64::MAX)).is_none());
  }
}
