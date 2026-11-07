//! Fan monitoring from HWMON `fanN_input` channels.
//!
//! Values are displayed exactly as the kernel reports them. Where kernel
//! values are derived from ACPI fan state rather than a hardware tachometer
//! (detected via the chip name `acpi_fan`), that caveat is attached to the
//! reading. No RPM is ever estimated from temperature.

use crate::fs::{self, SourcePaths};
use nuc_core::FanReading;
use std::path::PathBuf;

pub struct FanMonitor {
    paths: SourcePaths,
    fans: Option<Vec<Channel>>,
    last_discovery: std::time::Instant,
    /// Channel ids measured as slow (acpi_fan reads ~200 ms via ACPI on this
    /// platform); those are re-read every `slow_refresh`, cached otherwise
    /// with an explicit note — no value is presented as live when it is not.
    slow_ids: std::collections::HashSet<String>,
    slow_cache: std::collections::HashMap<String, (std::time::Instant, FanReading, u64)>,
    slow_refresh: std::time::Duration,
}

/// A single fan channel read slower than this many ms is firmware-backed.
const SLOW_READ_MS: u64 = 50;
const SLOW_REFRESH_DEFAULT_SECS: u64 = 30;

#[derive(Clone)]
struct Channel {
    id: String,
    label: Option<String>,
    hwmon_dir: PathBuf,
    channel: u32,
    chip: String,
}

impl FanMonitor {
    pub fn new(paths: SourcePaths) -> Self {
        FanMonitor {
            paths,
            fans: None,
            last_discovery: std::time::Instant::now(),
            slow_ids: Default::default(),
            slow_cache: Default::default(),
            slow_refresh: std::time::Duration::from_secs(SLOW_REFRESH_DEFAULT_SECS),
        }
    }

    fn discover(&self) -> Vec<Channel> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.paths.class_hwmon) {
            for e in entries.filter_map(std::result::Result::ok) {
                let dir = e.path();
                let chip = fs::read_string_opt(&dir.join("name")).unwrap_or_default();
                let mut chans = Vec::new();
                if let Ok(files) = std::fs::read_dir(&dir) {
                    for f in files.filter_map(std::result::Result::ok) {
                        let name = f.file_name().to_string_lossy().into_owned();
                        if let Some(rest) = name.strip_prefix("fan")
                            && let Some(idx) = rest.strip_suffix("_input")
                            && let Ok(n) = idx.parse::<u32>()
                        {
                            chans.push(n);
                        }
                    }
                }
                chans.sort();
                for n in chans {
                    out.push(Channel {
                        id: format!("{chip}:{}/fan{n}", fs::hwmon_device_id(&dir).display()),
                        label: fs::read_string_opt(&dir.join(format!("fan{n}_label"))),
                        hwmon_dir: dir.clone(),
                        channel: n,
                        chip: chip.clone(),
                    });
                }
            }
        }
        out
    }

    pub fn sample_fans(&mut self) -> Vec<FanReading> {
        if self.last_discovery.elapsed() >= std::time::Duration::from_secs(30) {
            self.fans = None;
        }
        if self.fans.is_none() {
            self.fans = Some(self.discover());
            self.last_discovery = std::time::Instant::now();
        }
        let mut out = Vec::new();
        let mut stale = false;
        if let Some(channels) = self.fans.clone() {
            for c in channels {
                // measured-slow channel inside its refresh window
                if self.slow_ids.contains(&c.id)
                    && let Some((when, base, ms)) = self.slow_cache.get(&c.id)
                    && when.elapsed() < self.slow_refresh
                {
                    let mut f = base.clone();
                    let note = format!(
                        "cached {:.0}s ago (this driver reads ~{ms} ms per access — re-read every {} s)",
                        when.elapsed().as_secs_f64(),
                        self.slow_refresh.as_secs()
                    );
                    f.note = Some(match f.note {
                        Some(n) => format!("{n}; {note}"),
                        None => note,
                    });
                    out.push(f);
                    continue;
                }
                let t0 = std::time::Instant::now();
                let raw = fs::read_f64_opt(&c.hwmon_dir.join(format!("fan{}_input", c.channel)));
                let alarm = fs::read_i64_opt(&c.hwmon_dir.join(format!("fan{}_alarm", c.channel)))
                    .map(|v| v != 0);
                let read_ms = t0.elapsed().as_millis() as u64;
                if raw.is_none() {
                    stale = true;
                }
                // acpi_fan reports an ACPI-managed value, not a verified RPM.
                let note = (c.chip == "acpi_fan").then_some(
                    "value reported by the ACPI fan interface; it may represent the fan control state rather than a verified tachometer RPM".to_string(),
                );
                let mut reading = FanReading {
                    id: c.id.clone(),
                    label: c
                        .label
                        .clone()
                        .unwrap_or_else(|| format!("Fan {}", c.channel)),
                    rpm: raw,
                    source: c.chip.clone(),
                    note,
                    fault: alarm,
                };
                if read_ms >= SLOW_READ_MS {
                    self.slow_ids.insert(c.id.clone());
                    let slow_note = format!(
                        "firmware-backed driver (read took {read_ms} ms) — reading cached between {} s refreshes",
                        self.slow_refresh.as_secs()
                    );
                    reading.note = Some(match reading.note {
                        Some(n) => format!("{n}; {slow_note}"),
                        None => slow_note,
                    });
                    self.slow_cache.insert(
                        c.id.clone(),
                        (std::time::Instant::now(), reading.clone(), read_ms),
                    );
                }
                out.push(reading);
            }
        }
        if stale {
            self.fans = None;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SourcePaths;
    use std::fs;
    use std::path::Path;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn acpi_fan_is_caveated() {
        let root = std::env::temp_dir().join("nuc-fix-fan");
        let _ = fs::remove_dir_all(&root);
        let hw = root.join("sys/class/hwmon/hwmon1");
        write(&hw.join("name"), "acpi_fan");
        write(&hw.join("fan1_input"), "61");
        let mut m = FanMonitor::new(SourcePaths::fixture(&root));
        let fans = m.sample_fans();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].rpm, Some(61.0));
        assert!(fans[0].note.is_some(), "acpi_fan needs a source caveat");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_fans_is_empty() {
        let root = std::env::temp_dir().join("nuc-fix-fan-empty");
        let _ = fs::remove_dir_all(&root);
        let mut m = FanMonitor::new(SourcePaths::fixture(&root));
        assert!(m.sample_fans().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn slow_channels_are_cached_between_refreshes() {
        let root = std::env::temp_dir().join("nuc-fix-fan-slow-cache");
        let _ = fs::remove_dir_all(&root);
        let hw = root.join("sys/class/hwmon/hwmon1");
        write(&hw.join("name"), "acpi_fan");
        write(&hw.join("fan1_input"), "61");
        let mut m = FanMonitor::new(SourcePaths::fixture(&root));
        m.slow_refresh = std::time::Duration::from_secs(3600);
        // channel id as discover() builds it (canonicalized dir in fixtures)
        let dir = root.join("sys/class/hwmon/hwmon1");
        let id = format!(
            "acpi_fan:{}/fan1",
            crate::fs::hwmon_device_id(&dir).display()
        );
        // first state: channel marked slow + fresh cache entry
        m.slow_ids.insert(id.clone());
        m.slow_cache.insert(
            id.clone(),
            (
                std::time::Instant::now(),
                FanReading {
                    id: id.clone(),
                    label: "Fan 1".into(),
                    rpm: Some(61.0),
                    source: "acpi_fan".into(),
                    note: None,
                    fault: None,
                },
                203,
            ),
        );
        // value changes on disk; cached reading must be served
        write(&hw.join("fan1_input"), "99");
        let fans = m.sample_fans();
        assert_eq!(fans[0].rpm, Some(61.0), "served from cache while fresh");
        assert!(fans[0].note.clone().unwrap().contains("cached"));
        // expiry: refresh window smaller than cache age passes -> re-read
        m.slow_refresh = std::time::Duration::ZERO;
        let fans = m.sample_fans();
        assert_eq!(fans[0].rpm, Some(99.0), "re-read after cache expiry");
        let _ = fs::remove_dir_all(&root);
    }
}
