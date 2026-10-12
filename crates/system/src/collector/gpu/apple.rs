//! Read-only IOAccelerator snapshots without subprocesses or privileged IOReport access.
//!
//! PerformanceStatistics keys are optional driver properties, not a stable Apple API.
//! Apple Silicon memory readings describe shared system memory, not dedicated VRAM.

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::time::Instant;

use super::{GpuReadings, MAX_GPUS, REDISCOVER_AFTER};
use crate::collector::{Warnings, clean_text, stable_id};
use crate::{DeviceInventory, DeviceKind, MetricReading};

type CfRef = *const c_void;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
  fn IOServiceMatching(name: *const c_char) -> CfRef;
  fn IOServiceGetMatchingServices(port: u32, matching: CfRef, iterator: *mut u32) -> i32;
  fn IOIteratorNext(iterator: u32) -> u32;
  fn IOObjectRelease(object: u32) -> i32;
  fn IORegistryEntryGetPath(entry: u32, plane: *const c_char, path: *mut c_char) -> i32;
  fn IORegistryEntryGetName(entry: u32, name: *mut c_char) -> i32;
  fn IORegistryEntryCreateCFProperty(
    entry: u32,
    key: CfRef,
    allocator: CfRef,
    options: u32,
  ) -> CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
  fn CFRelease(value: CfRef);
  fn CFGetTypeID(value: CfRef) -> usize;
  fn CFDictionaryGetTypeID() -> usize;
  fn CFDictionaryGetValue(dictionary: CfRef, key: CfRef) -> CfRef;
  fn CFNumberGetTypeID() -> usize;
  fn CFNumberGetValue(number: CfRef, number_type: isize, value: *mut c_void) -> u8;
  fn CFStringGetTypeID() -> usize;
  fn CFStringCreateWithCString(allocator: CfRef, value: *const c_char, encoding: u32) -> CfRef;
  fn CFStringGetCString(value: CfRef, buffer: *mut c_char, buffer_size: isize, encoding: u32)
  -> u8;
  fn CFDataGetTypeID() -> usize;
  fn CFDataGetLength(value: CfRef) -> isize;
  fn CFDataGetBytePtr(value: CfRef) -> *const u8;
}

const UTF8: u32 = 0x0800_0100;

struct IoObject(u32);

impl Drop for IoObject {
  fn drop(&mut self) {
    // SAFETY: Every handle wrapped here was acquired from IOKit and is released once.
    unsafe {
      IOObjectRelease(self.0);
    }
  }
}

struct CfObject(CfRef);

impl CfObject {
  fn owned(value: CfRef) -> Option<Self> {
    (!value.is_null()).then_some(Self(value))
  }

  fn key(value: &CStr) -> Option<Self> {
    // SAFETY: The literal is NUL terminated; a null allocator selects the default.
    Self::owned(unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), UTF8) })
  }
}

impl Drop for CfObject {
  fn drop(&mut self) {
    // SAFETY: These objects come from Create functions and own one CF reference.
    unsafe {
      CFRelease(self.0);
    }
  }
}

pub(super) struct AppleCollector {
  devices: Vec<AppleDevice>,
  discovery_error: Option<&'static str>,
  last_discovery: Instant,
}

struct AppleDevice {
  entry: IoObject,
  inventory: DeviceInventory,
  shared_memory: bool,
}

impl AppleCollector {
  pub fn new() -> Self {
    let (devices, discovery_error) = match discover() {
      Ok(devices) => (devices, None),
      Err(error) => (Vec::new(), Some(error)),
    };
    Self {
      devices,
      discovery_error,
      last_discovery: Instant::now(),
    }
  }

  pub fn collect(
    &mut self,
    limit: usize,
    metrics: &mut BTreeMap<String, MetricReading>,
    warnings: &mut Warnings,
  ) -> Vec<DeviceInventory> {
    if self.last_discovery.elapsed() >= REDISCOVER_AFTER {
      match discover() {
        Ok(devices) => {
          self.devices = devices;
          self.discovery_error = None;
        }
        Err(error) => self.discovery_error = Some(error),
      }
      self.last_discovery = Instant::now();
    }
    if let Some(error) = self.discovery_error {
      warnings.push(error);
    }
    if self.devices.len() > limit {
      warnings.push("GPU inventory is limited to 64 devices.");
    }
    self
      .devices
      .iter()
      .take(limit)
      .map(|device| {
        let statistics = property(device.entry.0, c"PerformanceStatistics");
        let number = |key| {
          statistics
            .as_ref()
            .and_then(|dictionary| dictionary_number(dictionary, key))
        };
        let integer = |key| {
          statistics
            .as_ref()
            .and_then(|dictionary| dictionary_integer(dictionary, key))
        };
        let readings = GpuReadings {
          usage_percent: number(c"Device Utilization %").or_else(|| number(c"Device Utilization")),
          memory_used_bytes: if device.shared_memory {
            integer(c"In use system memory")
          } else {
            None
          },
          memory_allocated_bytes: if device.shared_memory {
            integer(c"Alloc system memory")
          } else {
            None
          },
          shared_memory: device.shared_memory,
          // Temperature/power require a separate provider; never invent unavailable values.
          ..Default::default()
        };
        readings.record(&device.inventory.device_id, metrics, warnings);
        device.inventory.clone()
      })
      .collect()
  }
}

fn discover() -> std::result::Result<Vec<AppleDevice>, &'static str> {
  // SAFETY: The class is a constant C string. GetMatchingServices consumes matching.
  let matching = unsafe { IOServiceMatching(c"IOAccelerator".as_ptr()) };
  if matching.is_null() {
    return Err("Apple GPU collection is unavailable: IOAccelerator matching failed.");
  }
  let mut iterator = 0;
  // Port 0 is the default IOKit main port. The matching reference is consumed even on failure.
  let status = unsafe { IOServiceGetMatchingServices(0, matching, &mut iterator) };
  if status != 0 || iterator == 0 {
    if iterator != 0 {
      // SAFETY: A returned iterator, if present, owns one IOKit reference.
      unsafe {
        IOObjectRelease(iterator);
      }
    }
    return Err("Apple GPU collection is unavailable: IOAccelerator enumeration failed.");
  }
  let iterator = IoObject(iterator);
  let mut devices = Vec::new();
  while devices.len() < MAX_GPUS {
    // SAFETY: The iterator remains owned and valid for this loop.
    let entry = unsafe { IOIteratorNext(iterator.0) };
    if entry == 0 {
      break;
    }
    let entry = IoObject(entry);
    let mut path = [0_i8; 512];
    // SAFETY: IOKit io_string_t is a 512-byte buffer; the handle and plane are valid.
    if unsafe { IORegistryEntryGetPath(entry.0, c"IOService".as_ptr(), path.as_mut_ptr()) } != 0 {
      continue;
    }
    let Some(path) = c_buffer(&path) else {
      continue;
    };
    let id = stable_id("apple", &[path.as_bytes()]);
    let mut name = [0_i8; 128];
    // SAFETY: IOKit io_name_t is a 128-byte buffer.
    let name = if unsafe { IORegistryEntryGetName(entry.0, name.as_mut_ptr()) } == 0 {
      c_buffer(&name).unwrap_or_else(|| "Apple GPU accelerator".into())
    } else {
      "Apple GPU accelerator".into()
    };
    let model = property(entry.0, c"model")
      .and_then(|value| cf_text(&value))
      .unwrap_or_else(|| name.clone());
    let shared_memory = model.starts_with("Apple") || name.starts_with("AGX");
    let inventory = DeviceInventory {
      device_id: id,
      kind: DeviceKind::Gpu,
      name: clean_text(&model, 1024),
      total_bytes: None,
      attributes: BTreeMap::from([
        ("provider".into(), "apple_iokit".into()),
        ("registry_path".into(), clean_text(&path, 1024)),
        (
          "memory_kind".into(),
          if shared_memory {
            "shared"
          } else {
            "unavailable"
          }
          .into(),
        ),
        ("temperature_status".into(), "unavailable".into()),
        ("power_status".into(), "unavailable".into()),
      ]),
    };
    devices.push(AppleDevice {
      entry,
      inventory,
      shared_memory,
    });
  }
  Ok(devices)
}

fn property(entry: u32, key: &CStr) -> Option<CfObject> {
  let key = CfObject::key(key)?;
  // SAFETY: The entry remains owned by the caller and the key is a live CFString.
  CfObject::owned(unsafe { IORegistryEntryCreateCFProperty(entry, key.0, ptr::null(), 0) })
}

fn dictionary_value(dictionary: &CfObject, key: &CStr) -> Option<CfRef> {
  // SAFETY: Both CF objects are live; inspect the type before using dictionary APIs.
  if unsafe { CFGetTypeID(dictionary.0) != CFDictionaryGetTypeID() } {
    return None;
  }
  let key = CfObject::key(key)?;
  // The returned value is borrowed from dictionary, which remains live in the caller.
  let value = unsafe { CFDictionaryGetValue(dictionary.0, key.0) };
  (!value.is_null()).then_some(value)
}

fn dictionary_number(dictionary: &CfObject, key: &CStr) -> Option<f64> {
  let number = dictionary_value(dictionary, key)?;
  // SAFETY: The value is a live borrowed CF object; only CFNumber is converted.
  if unsafe { CFGetTypeID(number) != CFNumberGetTypeID() } {
    return None;
  }
  let mut value = 0_f64;
  // 6 is kCFNumberFloat64Type; storage points to an f64 of exactly that type.
  (unsafe { CFNumberGetValue(number, 6, (&mut value as *mut f64).cast()) } != 0)
    .then_some(value)
    .filter(|value| value.is_finite())
}

fn dictionary_integer(dictionary: &CfObject, key: &CStr) -> Option<u64> {
  let number = dictionary_value(dictionary, key)?;
  // SAFETY: The borrowed value remains live and is converted only after a type check.
  if unsafe { CFGetTypeID(number) != CFNumberGetTypeID() } {
    return None;
  }
  let mut value = 0_i64;
  // 4 is kCFNumberSInt64Type; refuse negative or unrepresentable byte counts.
  if unsafe { CFNumberGetValue(number, 4, (&mut value as *mut i64).cast()) } == 0 {
    return None;
  }
  value.try_into().ok()
}

fn cf_text(value: &CfObject) -> Option<String> {
  // SAFETY: The object is live, and each API is called only for its matching type.
  unsafe {
    if CFGetTypeID(value.0) == CFStringGetTypeID() {
      let mut buffer = [0_i8; 1024];
      if CFStringGetCString(value.0, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) != 0 {
        return c_buffer(&buffer);
      }
    } else if CFGetTypeID(value.0) == CFDataGetTypeID() {
      let length = CFDataGetLength(value.0);
      let bytes = CFDataGetBytePtr(value.0);
      if (1..=1024).contains(&length) && !bytes.is_null() {
        let bytes = std::slice::from_raw_parts(bytes, length as usize);
        let bytes = bytes.split(|byte| *byte == 0).next()?;
        return Some(String::from_utf8_lossy(bytes).into_owned());
      }
    }
  }
  None
}

fn c_buffer(buffer: &[i8]) -> Option<String> {
  let end = buffer.iter().position(|value| *value == 0)?;
  let bytes: Vec<_> = buffer[..end].iter().map(|value| *value as u8).collect();
  Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn apple_inventory_and_optional_snapshots_work_without_privileges() {
    let mut collector = AppleCollector::new();
    let mut metrics = BTreeMap::new();
    let mut warnings = Warnings::default();
    for device in collector.collect(64, &mut metrics, &mut warnings) {
      assert_eq!(device.attributes["provider"], "apple_iokit");
      assert!(device.total_bytes.is_none());
      assert!(!metrics.contains_key(&format!("gpu/{}/memory_total_bytes", device.device_id)));
    }
    assert!(metrics.values().all(|reading| reading.value.is_finite()));
  }
}
