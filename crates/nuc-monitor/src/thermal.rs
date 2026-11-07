//! Thermal sensor discovery over the documented HWMON sysfs ABI plus
//! `/sys/class/thermal` zones that lack an hwmon representation.
//!
//! Sensor identity is stable: chip device path + temp channel index. Missing
//! readings surface as `value_c: None` — the UI shows "Unavailable", never a
//! fabricated number.

use crate::fs::{self, SourcePaths};
use nuc_core::{AppError, Temperature, millidegree_to_celsius};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct SensorMeta {
    /// Stable sensor id, e.g. "coretemp/temp1".
    pub id: String,
    /// hwmon chip name (`coretemp`, `nvme`, `acpitz`, ...).
    pub chip: String,
    pub hwmon_dir: PathBuf,
    /// temp channel number from `tempN_input`.
    pub channel: u32,
    pub label: String,
    pub has_crit: bool,
    pub has_max: bool,
}

/// Channels whose read took at least this many milliseconds are treated as
/// firmware-backed (INT340x ACPI zones measure ~100 ms, acpi_fan ~200 ms on
/// this platform) and are re-read only every `slow_refresh`; intervening
/// samples carry the last real reading plus an honest "cached" note.
const SLOW_READ_MS: u64 = 50;
const SLOW_REFRESH_DEFAULT_SECS: u64 = 30;

pub struct ThermalMonitor {
    paths: SourcePaths,
    sensors: Option<Vec<SensorMeta>>,
    discovery_runs: u64,
    last_discovery: std::time::Instant,
    /// Channel ids measured as slow (perf note: reads block on ACPI calls).
    slow_ids: std::collections::HashSet<String>,
    /// Last real reading for slow channels: id -> (when, reading, read_ms).
    slow_cache: std::collections::HashMap<String, (std::time::Instant, Temperature, u64)>,
    slow_refresh: std::time::Duration,
}

/// Collect hwmon (or thermal-zone) directories in deterministic channel order.
fn list_dirs(dir: &std::path::Path, prefix: &str) -> Vec<PathBuf> {
    let mut out: Vec<(u32, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Ok(name) = e.file_name().into_string()
                && let Some(rest) = name.strip_prefix(prefix)
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_digit())
                && let Ok(idx) = rest.parse::<u32>()
            {
                out.push((idx, e.path()));
            }
        }
    }
    out.sort_by_key(|(idx, _)| *idx);
    out.into_iter().map(|(_, p)| p).collect()
}

fn list_files(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// One temperature channel -> model Temperature (None when unreadable).
fn read_channel(m: &SensorMeta) -> Option<Temperature> {
    let value = fs::read_i64_opt(&m.hwmon_dir.join(format!("temp{}_input", m.channel)))
        .map(millidegree_to_celsius);
    let max = fs::read_i64_opt(&m.hwmon_dir.join(format!("temp{}_max", m.channel)))
        .map(millidegree_to_celsius);
    let crit = fs::read_i64_opt(&m.hwmon_dir.join(format!("temp{}_crit", m.channel)))
        .map(millidegree_to_celsius);
    let alarm = if m.has_crit {
        fs::read_i64_opt(&m.hwmon_dir.join(format!("temp{}_crit_alarm", m.channel))).unwrap_or(0)
            != 0
    } else {
        false
    };
    value.map(|v| Temperature {
        id: m.id.clone(),
        label: m.label.clone(),
        value_c: Some(v),
        warn_c: max.or(crit),
        crit_c: crit,
        max_c: max,
        alarm,
        source: m.chip.clone(),
        note: None,
    })
}

impl ThermalMonitor {
    pub fn new(paths: SourcePaths) -> Self {
        ThermalMonitor {
            paths,
            sensors: None,
            discovery_runs: 0,
            last_discovery: std::time::Instant::now(),
            slow_ids: Default::default(),
            slow_cache: Default::default(),
            slow_refresh: std::time::Duration::from_secs(SLOW_REFRESH_DEFAULT_SECS),
        }
    }

    pub fn discovery_runs(&self) -> u64 {
        self.discovery_runs
    }

    /// Discover hwmon temperature channels once and cache the list.
    pub fn discover(&mut self) -> Result<&[SensorMeta], AppError> {
        if self.sensors.is_none() {
            self.sensors = Some(self.discover_channels());
        }
        Ok(self.sensors.as_deref().unwrap())
    }

    fn discover_channels(&mut self) -> Vec<SensorMeta> {
        self.discovery_runs += 1;
        self.last_discovery = std::time::Instant::now();
        let mut metas = Vec::new();
        for hwmon_dir in list_dirs(&self.paths.class_hwmon, "hwmon") {
            let chip = fs::read_string_opt(&hwmon_dir.join("name")).unwrap_or_default();
            if chip.is_empty() {
                continue;
            }
            let mut channels: BTreeMap<u32, ()> = BTreeMap::new();
            for name in list_files(&hwmon_dir) {
                if let Some(rest) = name.strip_prefix("temp")
                    && let Some(idx) = rest.strip_suffix("_input")
                    && let Ok(n) = idx.parse::<u32>()
                {
                    channels.insert(n, ());
                }
            }
            for idx in channels.keys().copied() {
                let label = fs::read_string_opt(&hwmon_dir.join(format!("temp{idx}_label")))
                    .unwrap_or_else(|| format!("temp{idx}"));
                metas.push(SensorMeta {
                    id: format!(
                        "{chip}:{}/temp{idx}",
                        fs::hwmon_device_id(&hwmon_dir).display()
                    ),
                    chip: chip.clone(),
                    hwmon_dir: hwmon_dir.clone(),
                    channel: idx,
                    label,
                    has_crit: hwmon_dir.join(format!("temp{idx}_crit")).exists(),
                    has_max: hwmon_dir.join(format!("temp{idx}_max")).exists(),
                });
            }
        }
        metas
    }

    /// Read all chips; thermal zones without hwmon representation are
    /// appended by their zone type. Unreadable channel -> `value_c: None`
    /// and invalidates the cache (sensors can appear/disappear).
    ///
    /// Firmware-backed channels (measured read >= `SLOW_READ_MS`) are only
    /// re-read every `slow_refresh`; in between the last real reading is
    /// served with an explicit "cached" note so no value is ever presented
    /// as live that is not.
    pub fn sample_temperatures(&mut self) -> Vec<Temperature> {
        if self.last_discovery.elapsed() >= std::time::Duration::from_secs(30) {
            self.sensors = None;
        }
        if self.sensors.is_none() {
            self.discover().ok();
        }
        let mut out = Vec::new();
        let mut stale = false;
        if let Some(metas) = self.sensors.clone() {
            for m in metas {
                // Previously measured slow channel inside its refresh window
                if self.slow_ids.contains(&m.id)
                    && let Some((when, base, ms)) = self.slow_cache.get(&m.id)
                    && when.elapsed() < self.slow_refresh
                {
                    let mut t = base.clone();
                    let note = format!(
                        "cached {:.0}s ago (this sensor reads ~{ms} ms per access — re-read every {} s)",
                        when.elapsed().as_secs_f64(),
                        self.slow_refresh.as_secs()
                    );
                    t.note = Some(match t.note {
                        Some(n) => format!("{n}; {note}"),
                        None => note,
                    });
                    out.push(t);
                    continue;
                }
                let t0 = std::time::Instant::now();
                match read_channel(&m) {
                    Some(mut t) => {
                        let ms = t0.elapsed().as_millis() as u64;
                        if ms >= SLOW_READ_MS {
                            self.slow_ids.insert(m.id.clone());
                            let note = format!(
                                "firmware-backed sensor (read took {ms} ms) — reading cached between {} s refreshes",
                                self.slow_refresh.as_secs()
                            );
                            t.note = Some(match t.note {
                                Some(n) => format!("{n}; {note}"),
                                None => note,
                            });
                            self.slow_cache
                                .insert(m.id.clone(), (std::time::Instant::now(), t.clone(), ms));
                        }
                        out.push(t);
                    }
                    None => {
                        out.push(Temperature {
                            id: m.id.clone(),
                            label: m.label.clone(),
                            value_c: None,
                            warn_c: None,
                            crit_c: None,
                            max_c: None,
                            alarm: false,
                            source: m.chip.clone(),
                            note: Some("sensor disappeared or unreadable".into()),
                        });
                        stale = true;
                    }
                }
            }
        }
        if stale {
            self.sensors = None;
        }
        for zone in list_dirs(&self.paths.class_thermal, "thermal_zone") {
            let Some(zone_type) = fs::read_string_opt(&zone.join("type")) else {
                continue;
            };
            let has_hwmon = std::fs::read_dir(&zone)
                .into_iter()
                .flatten()
                .filter_map(std::result::Result::ok)
                .any(|e| e.file_name().to_string_lossy().starts_with("hwmon"));
            if has_hwmon {
                continue; // already listed via /sys/class/hwmon
            }
            let id = format!(
                "thermal-zone/{}/{zone_type}",
                zone.file_name().unwrap().to_string_lossy()
            );
            // slow-channel handling: measure and cache the zone temp read
            if self.slow_ids.contains(&id)
                && let Some((when, base, ms)) = self.slow_cache.get(&id)
                && when.elapsed() < self.slow_refresh
            {
                let mut t = base.clone();
                let note = format!(
                    "cached {:.0}s ago (this sensor reads ~{ms} ms per access — re-read every {} s)",
                    when.elapsed().as_secs_f64(),
                    self.slow_refresh.as_secs()
                );
                t.note = Some(match t.note {
                    Some(n) => format!("{n}; {note}"),
                    None => note,
                });
                out.push(t);
                continue;
            }
            let t0 = std::time::Instant::now();
            let value = fs::read_i64_opt(&zone.join("temp")).map(millidegree_to_celsius);
            let read_ms = t0.elapsed().as_millis() as u64;
            let mut t = Temperature {
                id,
                label: zone_type.clone(),
                value_c: value,
                warn_c: None,
                crit_c: None,
                max_c: None,
                alarm: false,
                source: "thermal_zone".into(),
                note: Some("no driver limits exposed".into()),
            };
            if read_ms >= SLOW_READ_MS {
                self.slow_ids.insert(t.id.clone());
                let note = format!(
                    "firmware-backed zone (read took {read_ms} ms) — reading cached between {} s refreshes",
                    self.slow_refresh.as_secs()
                );
                t.note = Some(match t.note {
                    Some(n) => format!("{n}; {note}"),
                    None => note,
                });
                self.slow_cache.insert(
                    t.id.clone(),
                    (std::time::Instant::now(), t.clone(), read_ms),
                );
            }
            out.push(t);
        }
        if out.len() > 64 {
            out.truncate(64);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn discovery_and_limits() {
        let root = std::env::temp_dir().join("nuc-fix-thermal-basic");
        let _ = fs::remove_dir_all(&root);
        let hw = root.join("sys/class/hwmon/hwmon0");
        write(&hw.join("name"), "coretemp");
        write(&hw.join("temp1_label"), "Package id 0");
        write(&hw.join("temp1_input"), "56000");
        write(&hw.join("temp1_crit"), "100000");
        write(&hw.join("temp1_max"), "80000");
        let mut m = ThermalMonitor::new(SourcePaths::fixture(&root));
        let temps = m.sample_temperatures();
        assert_eq!(temps.len(), 1);
        assert_eq!(temps[0].label, "Package id 0");
        assert_eq!(temps[0].value_c, Some(56.0));
        assert_eq!(temps[0].crit_c, Some(100.0));
        assert_eq!(temps[0].max_c, Some(80.0));
        assert_eq!(temps[0].warn_c, Some(80.0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn zone_without_hwmon() {
        let root = std::env::temp_dir().join("nuc-fix-thermal-zone");
        let _ = fs::remove_dir_all(&root);
        write(
            &root.join("sys/class/thermal/thermal_zone7/type"),
            "x86_pkg_temp",
        );
        write(&root.join("sys/class/thermal/thermal_zone7/temp"), "61000");
        let mut m = ThermalMonitor::new(SourcePaths::fixture(&root));
        let temps = m.sample_temperatures();
        assert_eq!(temps.len(), 1);
        assert_eq!(temps[0].id, "thermal-zone/thermal_zone7/x86_pkg_temp");
        assert_eq!(temps[0].value_c, Some(61.0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_reading_shown_unavailable() {
        let root = std::env::temp_dir().join("nuc-fix-thermal-missing");
        let _ = fs::remove_dir_all(&root);
        let hw = root.join("sys/class/hwmon/hwmon0");
        write(&hw.join("name"), "coretemp");
        write(&hw.join("temp1_label"), "Package id 0");
        // temp1_input intentionally missing
        let mut m = ThermalMonitor::new(SourcePaths::fixture(&root));
        let temps = m.sample_temperatures();
        assert!(
            temps.is_empty(),
            "no tempN_input file => channel not listed"
        );
        assert_eq!(m.discovery_runs(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_chip_names_have_distinct_ids_and_hotplug_is_rediscovered() {
        let root = std::env::temp_dir().join("nuc-thermal-multiple");
        let _ = fs::remove_dir_all(&root);
        let hw = root.join("sys/class/hwmon/hwmon0");
        write(&hw.join("name"), "nvme");
        write(&hw.join("temp1_input"), "40000");
        let mut monitor = ThermalMonitor::new(SourcePaths::fixture(&root));
        assert_eq!(monitor.sample_temperatures().len(), 1);
        let hw = root.join("sys/class/hwmon/hwmon1");
        write(&hw.join("name"), "nvme");
        write(&hw.join("temp1_input"), "90000");
        monitor.last_discovery -= std::time::Duration::from_secs(31);
        let temps = monitor.sample_temperatures();
        assert_eq!(temps.len(), 2);
        assert_ne!(temps[0].id, temps[1].id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn slow_zones_are_cached_between_refreshes() {
        let root = std::env::temp_dir().join("nuc-fix-thermal-slow-cache");
        let _ = fs::remove_dir_all(&root);
        let zone = root.join("sys/class/thermal/thermal_zone2");
        write(&zone.join("type"), "INT340x");
        write(&zone.join("temp"), "45000");
        let mut m = ThermalMonitor::new(SourcePaths::fixture(&root));
        let id = "thermal-zone/thermal_zone2/INT340x";
        // first state: zone marked slow + fresh cache entry (as set by a
        // measured ≥ SLOW_READ_MS read)
        m.slow_ids.insert(id.to_string());
        m.slow_cache.insert(
            id.to_string(),
            (
                std::time::Instant::now(),
                Temperature {
                    id: id.into(),
                    label: "INT340x".into(),
                    value_c: Some(45.0),
                    warn_c: None,
                    crit_c: None,
                    max_c: None,
                    alarm: false,
                    source: "thermal_zone".into(),
                    note: None,
                },
                103,
            ),
        );
        // value changes on disk; cached reading must be served while fresh
        write(&zone.join("temp"), "99000");
        let temps = m.sample_temperatures();
        assert_eq!(temps.len(), 1);
        assert_eq!(
            temps[0].value_c,
            Some(45.0),
            "served from cache while fresh"
        );
        let note = temps[0].note.clone().unwrap();
        assert!(note.contains("cached"), "note = {note:?}");
        // …and the fast-channel path must not prepend a cache note
        m.slow_refresh = std::time::Duration::ZERO;
        let temps = m.sample_temperatures();
        assert_eq!(temps[0].value_c, Some(99.0), "re-read after cache expiry");
        assert!(!temps[0].note.clone().unwrap().contains("cached"));
        let _ = fs::remove_dir_all(&root);
    }
}
