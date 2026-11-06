//! Intel RAPL power-cap monitoring via `/sys/class/powercap`.
//!
//! Power is only shown as an energy delta over the sampling interval — it is
//! *processor* package/core/uncore energy, not total wall-socket power.
//! The overlapping `intel-rapl-mmio` view of package-0 is excluded. Zones are
//! reported separately (never summed): core/uncore are nested within the
//! package measurement.

use crate::fs::{self, SourcePaths};
use nuc_core::{RaplConstraint, RaplZone, energy_uj_to_watts};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

pub struct RaplMonitor {
    paths: SourcePaths,
    /// Baseline (energy_uj, timestamp) per zone, updated after each sample.
    prev: Mutex<HashMap<String, (u64, Instant)>>,
}

struct ZonePaths {
    key: String,
    label: String,
    dir: std::path::PathBuf,
    subdomains: Vec<(String, String, std::path::PathBuf)>,
}

impl RaplMonitor {
    pub fn new(paths: SourcePaths) -> Self {
        RaplMonitor {
            paths,
            prev: Mutex::new(HashMap::new()),
        }
    }

    /// Enumerate `intel-rapl:<n>` zones with their `intel-rapl:<n>:<m>`
    /// sub-domains, in deterministic order. `intel-rapl-mmio` is skipped as a
    /// duplicate of package-0.
    fn enumerate(&self) -> Vec<ZonePaths> {
        let class = &self.paths.class_powercap;
        let mut top: Vec<(u32, std::path::PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(class) {
            for e in entries.flatten() {
                if let Ok(name) = e.file_name().into_string() {
                    // top-level: "intel-rapl:<digits>" exactly (no mmio, no sub id)
                    if let Some(rest) = name.strip_prefix("intel-rapl:")
                        && !name.contains("mmio")
                        && !rest.contains(':')
                        && let Ok(n) = rest.parse::<u32>()
                    {
                        top.push((n, e.path()));
                    }
                }
            }
        }
        top.sort_by_key(|(n, _)| *n);
        let mut out = Vec::new();
        for (_, dir) in top {
            let label = fs::read_string_opt(&dir.join("name")).unwrap_or_default();
            if label.is_empty() {
                continue;
            }
            let key = dir
                .file_name()
                .and_then(|f| f.to_str().map(String::from))
                .unwrap_or_default();
            let mut subdomains = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    if let Ok(sub) = e.file_name().into_string()
                        && let Some(rest) = sub.strip_prefix(&format!("{key}:"))
                        && !rest.is_empty()
                        && rest.parse::<u32>().is_ok()
                        && rest.split(':').count() == 1
                        && let Some(sub_label) = fs::read_string_opt(&e.path().join("name"))
                    {
                        subdomains.push((sub, sub_label, e.path()));
                    }
                }
            }
            subdomains.sort();
            out.push(ZonePaths {
                key: key.clone(),
                label,
                dir,
                subdomains,
            });
        }
        out
    }

    fn read_constraints(base: &std::path::Path) -> Vec<RaplConstraint> {
        (0..3)
            .filter_map(|i| {
                let name = fs::read_string_opt(&base.join(format!("constraint_{i}_name")))?;
                if name.is_empty() {
                    return None;
                }
                Some(RaplConstraint {
                    name,
                    power_limit_uw: fs::read_u64_opt(
                        &base.join(format!("constraint_{i}_power_limit_uw")),
                    ),
                    max_power_uw: fs::read_u64_opt(
                        &base.join(format!("constraint_{i}_max_power_uw")),
                    ),
                    time_window_us: fs::read_u64_opt(
                        &base.join(format!("constraint_{i}_time_window_us")),
                    ),
                    // Source includes limits as telemetry only. Kernel-level
                    // writes go through a separate privileged decision, never
                    // silently from the monitor thread.
                    writable: false,
                })
            })
            .collect()
    }

    /// Interval power for one zone: wrap-aware energy delta / elapsed time.
    /// Wrap handling uses `max_energy_range_uj`; suspend gaps invalidate the
    /// interval ("deltas_valid" false).
    fn power_for(
        prev: &mut HashMap<String, (u64, Instant)>,
        next: &mut HashMap<String, (u64, Instant)>,
        key: &str,
        energy: Option<u64>,
        max_range: Option<u64>,
        now: Instant,
        deltas_valid: bool,
    ) -> Option<f64> {
        match energy {
            Some(cur) => {
                let old = prev.remove(key);
                next.insert(key.to_string(), (cur, now));
                let Some((p_energy, p_ts)) = old else {
                    return None; // first sample: baseline only
                };
                if !deltas_valid {
                    return None;
                }
                let dt = now.saturating_duration_since(p_ts).as_secs_f64();
                if cur >= p_energy {
                    energy_uj_to_watts(cur - p_energy, dt)
                } else if let Some(r) = max_range {
                    // counter wrapped around its maximum range
                    energy_uj_to_watts(r.saturating_sub(p_energy).saturating_add(cur), dt)
                } else {
                    None // unknown-range reset: interval invalid
                }
            }
            None => None, // a failed read breaks the measurement interval
        }
    }

    /// Sample all readable RAPL domains. Package and its sub-domains are
    /// returned as separate entries; the caller must not sum nested domains.
    pub fn sample(&self, deltas_valid: bool) -> Vec<RaplZone> {
        let now = Instant::now();
        let mut zones = Vec::new();
        let mut prev = self.prev.lock().unwrap();
        let mut taken = std::mem::take(&mut *prev);
        let mut next = HashMap::new();
        for z in self.enumerate() {
            let energy = fs::read_u64_opt(&z.dir.join("energy_uj"));
            let max_range = fs::read_u64_opt(&z.dir.join("max_energy_range_uj"));
            let power = Self::power_for(
                &mut taken,
                &mut next,
                &z.key,
                energy,
                max_range,
                now,
                deltas_valid,
            );
            zones.push(RaplZone {
                id: z.key.clone(),
                name: z.label.clone(),
                energy_uj: energy,
                max_energy_range_uj: max_range,
                power_w: power,
                constraints: Self::read_constraints(&z.dir),
            });
            for (sub_key, sub_label, sub_dir) in &z.subdomains {
                let energy = fs::read_u64_opt(&sub_dir.join("energy_uj"));
                let max_range = fs::read_u64_opt(&sub_dir.join("max_energy_range_uj"));
                let power = Self::power_for(
                    &mut taken,
                    &mut next,
                    sub_key,
                    energy,
                    max_range,
                    now,
                    deltas_valid,
                );
                zones.push(RaplZone {
                    id: sub_key.clone(),
                    name: sub_label.clone(),
                    energy_uj: energy,
                    max_energy_range_uj: max_range,
                    power_w: power,
                    constraints: Self::read_constraints(sub_dir),
                });
            }
        }
        *prev = next;
        zones
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
    fn package_power_and_subdomains() {
        let root = std::env::temp_dir().join("nuc-fix-rapl-power");
        let _ = fs::remove_dir_all(&root);
        let pkg = root.join("sys/class/powercap/intel-rapl:0");
        let core = root.join("sys/class/powercap/intel-rapl:0/intel-rapl:0:0");
        write(&pkg.join("name"), "package-0");
        write(&pkg.join("max_energy_range_uj"), "2000000");
        write(&pkg.join("energy_uj"), "1000000");
        write(&pkg.join("constraint_0_name"), "long_term");
        write(&pkg.join("constraint_0_power_limit_uw"), "20000000");
        write(&core.join("name"), "core");
        write(&core.join("energy_uj"), "500000");
        write(&core.join("max_energy_range_uj"), "2000000");
        let m = RaplMonitor::new(SourcePaths::fixture(&root));
        let first = m.sample(true);
        assert_eq!(first.len(), 2);
        assert!(first[0].power_w.is_none()); // baseline
        assert_eq!(first[0].constraints.len(), 1);
        // 200_000 µJ increase
        write(&pkg.join("energy_uj"), "1200000");
        write(&core.join("energy_uj"), "560000");
        let second = m.sample(true);
        let pkg = &second[0];
        let p = pkg.power_w.unwrap();
        assert!(p > 0.0);
        assert!(
            second[1].power_w.is_some(),
            "sub-domain power computed independently"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn suspend_gap_invalidates() {
        let root = std::env::temp_dir().join("nuc-fix-rapl-gap");
        let _ = fs::remove_dir_all(&root);
        let pkg = root.join("sys/class/powercap/intel-rapl:0");
        write(&pkg.join("name"), "package-0");
        write(&pkg.join("energy_uj"), "1000000");
        let m = RaplMonitor::new(SourcePaths::fixture(&root));
        let _ = m.sample(true);
        write(&pkg.join("energy_uj"), "1500000");
        let r = m.sample(false);
        assert!(r[0].power_w.is_none(), "gap must disable interval power");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wraparound_uses_range() {
        let root = std::env::temp_dir().join("nuc-fix-rapl-wrap");
        let _ = fs::remove_dir_all(&root);
        let pkg = root.join("sys/class/powercap/intel-rapl:0");
        write(&pkg.join("name"), "package-0");
        write(&pkg.join("max_energy_range_uj"), "1000000");
        write(&pkg.join("energy_uj"), "990000");
        let m = RaplMonitor::new(SourcePaths::fixture(&root));
        let _ = m.sample(true);
        // wrapped: measured delta = (1000000-990000)+20000 = 30000 µJ
        write(&pkg.join("energy_uj"), "20000");
        let s = m.sample(true);
        let p = s[0].power_w.unwrap();
        // wrapped delta = 30_000 µJ over a tiny timer interval: always positive
        assert!(p > 0.0, "wrapped delta yields a positive power, got {p}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mmio_duplicate_excluded() {
        let root = std::env::temp_dir().join("nuc-fix-rapl-mmio");
        let _ = fs::remove_dir_all(&root);
        write(
            &root.join("sys/class/powercap/intel-rapl:0/name"),
            "package-0",
        );
        write(
            &root.join("sys/class/powercap/intel-rapl-mmio:0/name"),
            "package-0",
        );
        let m = RaplMonitor::new(SourcePaths::fixture(&root));
        let zones = m.sample(true);
        assert_eq!(zones.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_energy_never_creates_a_zero_baseline() {
        let mut prev = HashMap::new();
        let mut next = HashMap::new();
        let now = Instant::now();
        assert!(
            RaplMonitor::power_for(&mut prev, &mut next, "pkg", None, None, now, true).is_none()
        );
        assert!(next.is_empty());
        assert!(
            RaplMonitor::power_for(
                &mut next,
                &mut prev,
                "pkg",
                Some(5_000_000),
                None,
                now + std::time::Duration::from_secs(2),
                true
            )
            .is_none()
        );
    }
}
