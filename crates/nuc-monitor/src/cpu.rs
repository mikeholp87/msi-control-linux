//! CPU utilization, frequency policy and throttling counters.
//!
//! Utilization uses `/proc/stat` jiffy deltas. The first sample and sampling
//! gaps (suspend, startup) yield `None` instead of a fabricated percentage.

use crate::fs::{self, SourcePaths};
use nuc_core::{CpuPolicy, CpuSnapshot, DeviceInfo, ThrottleInfo, Topology, energy_uj_to_watts};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Default)]
struct Jiffies {
    /// idle + iowait (kernel counts iowait in the idle bucket for utilization)
    idle: u64,
    total: u64,
}

#[derive(Clone, Default)]
struct PrevSample {
    per_cpu: BTreeMap<u32, Jiffies>,
    total: Jiffies,
}

struct PrevEnergy {
    energy_uj: Option<u64>,
    ts: Option<Instant>,
}

/// Parse `/proc/stat`: aggregated line first, then per-CPU lines.
fn parse_cpu_stats(proc_dir: &std::path::Path) -> Option<(Jiffies, Vec<(u32, Jiffies)>)> {
    let text = fs::read_string(&proc_dir.join("stat")).ok()?;
    let mut agg: Option<Jiffies> = None;
    let mut per_cpu: Vec<(u32, Jiffies)> = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let head = fields.next()?;
        if !head.starts_with("cpu") {
            continue;
        }
        let nums: Vec<u64> = fields.take(8).filter_map(|f| f.parse().ok()).collect();
        if nums.len() < 5 {
            continue;
        }
        let j = Jiffies {
            idle: nums[3].saturating_add(nums[4]),
            total: nums.iter().sum(),
        };
        if head == "cpu" {
            if agg.is_none() {
                agg = Some(j);
            }
        } else if let Ok(id) = head[3..].parse::<u32>() {
            per_cpu.push((id, j));
        }
    }
    Some((agg?, per_cpu))
}

pub struct CpuMonitor {
    paths: SourcePaths,
    topology: Topology,
    /// Snapshot of counters taken on the *previous* successful sample.
    prev_usage: Mutex<Option<PrevSample>>,
    prev_throttle: Mutex<(Option<u64>, Option<u64>)>,
    prev_energy: Mutex<PrevEnergy>,
}

impl CpuMonitor {
    pub fn new(paths: SourcePaths, topology: Topology) -> Self {
        CpuMonitor {
            paths,
            topology,
            prev_usage: Mutex::new(None),
            prev_throttle: Mutex::new((None, None)),
            prev_energy: Mutex::new(PrevEnergy {
                energy_uj: None,
                ts: None,
            }),
        }
    }

    pub fn topology(&self) -> &Topology {
        &self.topology
    }

    pub fn cpu_model(paths: &SourcePaths) -> DeviceInfo {
        let mut model = String::new();
        let mut vendor = String::new();
        if let Ok(text) = fs::read_string(&paths.proc.join("cpuinfo")) {
            for line in text.lines() {
                if line.starts_with("model name") {
                    model = line
                        .split_once(':')
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default();
                } else if line.starts_with("vendor_id") {
                    vendor = line
                        .split_once(':')
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default();
                }
                if !model.is_empty() && !vendor.is_empty() {
                    break;
                }
            }
        }
        DeviceInfo { model, vendor }
    }

    /// Utilization from jiffy deltas. `deltas_valid == false` (first sample,
    /// resume from suspend) yields `(None, _)`.
    fn sample_usage(&self, deltas_valid: bool) -> (Option<f64>, Vec<Option<f64>>) {
        let Some((agg, per_cpu)) = parse_cpu_stats(&self.paths.proc) else {
            *self.prev_usage.lock().unwrap() = None;
            return (None, Vec::new());
        };
        let current = PrevSample {
            per_cpu: per_cpu.iter().cloned().collect(),
            total: agg,
        };
        let saved_prev: Option<PrevSample> = {
            let mut slot = self.prev_usage.lock().unwrap();
            let old = slot.clone();
            *slot = Some(current);
            old
        };
        let Some(prev) = saved_prev else {
            return (None, Vec::new()); // first sample establishes baseline
        };
        if !deltas_valid {
            return (None, Vec::new());
        }
        let d_total = agg.total.saturating_sub(prev.total.total) as f64;
        if d_total <= 0.0 {
            return (None, Vec::new());
        }
        let d_idle = agg.idle.saturating_sub(prev.total.idle) as f64;
        let total_pct = (1.0 - d_idle / d_total).clamp(0.0, 1.0) * 100.0;

        let mut per: Vec<Option<f64>> = Vec::with_capacity(per_cpu.len());
        for (id, cur) in &per_cpu {
            match prev.per_cpu.get(id) {
                Some(p) => {
                    let dt = cur.total.saturating_sub(p.total) as f64;
                    if dt > 0.0 {
                        let di = cur.idle.saturating_sub(p.idle) as f64;
                        per.push(Some((1.0 - di / dt).clamp(0.0, 1.0) * 100.0));
                    } else {
                        per.push(None);
                    }
                }
                None => per.push(None), // CPU appeared via hotplug
            }
        }
        (Some(total_pct), per)
    }

    pub fn sample_freq_state(&self) -> nuc_core::CpuFreqState {
        let base = self.paths.cpu.join("cpufreq");
        let cpu0 = self.paths.cpu.join("cpu0/cpufreq");
        let mut state = nuc_core::CpuFreqState {
            running_driver: fs::read_string_opt(&base.join("policy0/scaling_driver"))
                .or_else(|| fs::read_string_opt(&base.join("scaling_driver")))
                .unwrap_or_default(),
            intel_pstate_mode: fs::read_string_opt(&self.paths.cpu.join("intel_pstate/status")),
            governor: fs::read_string_opt(&cpu0.join("scaling_governor")),
            available_governors: fs::read_string_opt(&cpu0.join("scaling_available_governors"))
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            epp: fs::read_string_opt(&cpu0.join("energy_performance_preference")),
            available_epps: fs::read_string_opt(
                &cpu0.join("energy_performance_available_preferences"),
            )
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
            turbo_enabled: None,
            policies: Vec::new(),
        };
        let no_turbo = self.paths.cpu.join("intel_pstate/no_turbo");
        let boost = self.paths.cpu.join("cpufreq/boost");
        state.turbo_enabled = if no_turbo.exists() {
            fs::read_string_opt(&no_turbo).map(|v| v != "1")
        } else if boost.exists() {
            fs::read_string_opt(&boost).map(|v| v == "1")
        } else {
            None
        };

        let mut policy_ids: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&base) {
            for e in entries.filter_map(std::result::Result::ok) {
                if let Ok(name) = e.file_name().into_string()
                    && name.starts_with("policy")
                {
                    policy_ids.push(name);
                }
            }
        }
        policy_ids.sort_by_key(|k| {
            k.trim_start_matches("policy")
                .parse::<u64>()
                .unwrap_or(u64::MAX)
        });

        // Hybrid CPUs expose one cpufreq policy per logical CPU. Collapse
        // policies that run with identical limits into one display entry and
        // union their CPU lists so nothing is lost.
        let mut merged: BTreeMap<(u64, u64, u64), CpuPolicy> = BTreeMap::new();
        for id in policy_ids {
            let p = base.join(&id);
            let cur = fs::read_u64_opt(&p.join("scaling_cur_freq"));
            let min = fs::read_u64_opt(&p.join("scaling_min_freq")).unwrap_or(0);
            let max = fs::read_u64_opt(&p.join("scaling_max_freq")).unwrap_or(0);
            let cpus: Vec<u32> = fs::read_string_opt(&p.join("affected_cpus"))
                .or_else(|| fs::read_string_opt(&p.join("related_cpus")))
                .map(|s| {
                    s.split_whitespace()
                        .filter_map(|x| x.parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            let entry_key = (min, max, cur.unwrap_or(0));
            let entry_exists = merged.contains_key(&entry_key);
            let entry = merged.entry(entry_key).or_insert_with(|| {
                let c = cpus.clone();
                CpuPolicy {
                    id: id.clone(),
                    cpus: c,
                    f_min_khz: (min > 0).then_some(min),
                    f_max_khz: (max > 0).then_some(max),
                    f_cur_khz: cur,
                    hw_pref_khz: fs::read_u64_opt(&p.join("hw_pref")),
                }
            });
            if entry_exists {
                entry.cpus.extend(cpus.iter().copied());
            }
        }
        state.policies = merged.into_values().collect();
        state
    }

    /// Throttle counters + delta since the previous sample (None on first).
    pub fn sample_throttle(&self) -> (ThrottleInfo, Option<u64>, Option<u64>) {
        let dir = self.paths.cpu.join("cpu0/thermal_throttle");
        let mut info = ThrottleInfo::default();
        if dir.exists() {
            info.supported = true;
            info.core_throttle_count = fs::read_u64_opt(&dir.join("core_throttle_count"));
            info.core_total_time_ms = fs::read_u64_opt(&dir.join("core_throttle_total_time_ms"));
            info.core_max_time_ms = fs::read_u64_opt(&dir.join("core_throttle_max_time_ms"));
            info.package_throttle_count = fs::read_u64_opt(&dir.join("package_throttle_count"));
            info.package_total_time_ms =
                fs::read_u64_opt(&dir.join("package_throttle_total_time_ms"));
            info.package_max_time_ms = fs::read_u64_opt(&dir.join("package_throttle_max_time_ms"));
        }
        let mut prev = self.prev_throttle.lock().unwrap();
        let new_core = info
            .core_throttle_count
            .zip(prev.0)
            .and_then(|(c, p)| c.checked_sub(p));
        let new_pkg = info
            .package_throttle_count
            .zip(prev.1)
            .and_then(|(c, p)| c.checked_sub(p));
        *prev = (info.core_throttle_count, info.package_throttle_count);
        (info, new_core, new_pkg)
    }

    pub fn read_package_energy_uj(&self) -> Option<u64> {
        fs::read_u64_opt(&self.paths.class_powercap.join("intel-rapl:0/energy_uj"))
    }

    /// Interval power (W) from consecutive package energy readings, with RAPL
    /// wraparound handled via `max_energy_range_uj`.
    pub fn package_interval_power(&self, deltas_valid: bool) -> Option<f64> {
        let mut prev = self.prev_energy.lock().unwrap();
        let Some(energy) = self.read_package_energy_uj() else {
            prev.energy_uj = None;
            prev.ts = None;
            return None;
        };
        let now = Instant::now();
        let (p_energy, p_ts) = (prev.energy_uj, prev.ts);
        prev.energy_uj = Some(energy);
        prev.ts = Some(now);
        let (Some(pe), Some(pts)) = (p_energy, p_ts) else {
            return None; // first reading establishes the baseline
        };
        let dt = now.saturating_duration_since(pts).as_secs_f64();
        let max_range = fs::read_u64_opt(
            &self
                .paths
                .class_powercap
                .join("intel-rapl:0/max_energy_range_uj"),
        );
        let delta = if energy >= pe {
            energy - pe
        } else if let Some(r) = max_range {
            r.saturating_sub(pe).saturating_add(energy)
        } else {
            return None; // counter reset without a known range
        };
        if !deltas_valid {
            return None;
        }
        energy_uj_to_watts(delta, dt)
    }

    pub fn sample(&self, deltas_valid: bool) -> CpuSnapshot {
        let (usage_total, usage) = self.sample_usage(deltas_valid);
        let (throttle, new_core, new_pkg) = self.sample_throttle();
        let power = self.package_interval_power(deltas_valid);
        CpuSnapshot {
            info: Self::cpu_model(&self.paths),
            topology: self.topology.clone(),
            usage,
            usage_total,
            freq: self.sample_freq_state(),
            package_power_w: power,
            interval_power_w: power,
            peak_power_w: None, // engine tracks running maximum
            throttle,
            new_core_throttles: new_core,
            new_package_throttles: new_pkg,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(p: &Path, content: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn first_sample_returns_none_then_deltas() {
        let root = std::env::temp_dir().join("nuc-fix-cpu-proc");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proc")).unwrap();
        let paths = SourcePaths::fixture(&root);
        write(
            &root.join("proc/stat"),
            "cpu  100 0 0 400 100 0 0 0 0 0\ncpu0 50 0 0 200 50 0 0 0\n",
        );
        let topo = Topology::default();
        let m = CpuMonitor::new(paths, topo);
        let first = m.sample_usage(true);
        assert!(first.0.is_none());
        // counters advanced
        write(
            &root.join("proc/stat"),
            "cpu  200 0 0 480 100 0 0 0 0 0\ncpu0 100 0 0 240 50 0 0 0\n",
        );
        let (total, per) = m.sample_usage(true);
        // d_total=780-600=180, d_idle=580-500=80 => busy=1-80/180
        let t = total.unwrap();
        assert!((t - (100.0 - 80.0 / 180.0 * 100.0)).abs() < 0.01, "got {t}");
        assert_eq!(per.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn suspend_gap_invalidates_usage() {
        let root = std::env::temp_dir().join("nuc-fix-cpu-proc2");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proc")).unwrap();
        write(&root.join("proc/stat"), "cpu  100 0 0 400 100 0 0\n");
        let m = CpuMonitor::new(SourcePaths::fixture(&root), Topology::default());
        let _ = m.sample_usage(true);
        write(&root.join("proc/stat"), "cpu  900 0 0 4000 100 0 0\n");
        assert!(m.sample_usage(false).0.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rapl_wraparound() {
        let root = std::env::temp_dir().join("nuc-fix-cpu-rapl");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sys/class/powercap/intel-rapl:0")).unwrap();
        write(
            &root.join("sys/class/powercap/intel-rapl:0/max_energy_range_uj"),
            "1000000",
        );
        let m = CpuMonitor::new(SourcePaths::fixture(&root), Topology::default());
        write(
            &root.join("sys/class/powercap/intel-rapl:0/energy_uj"),
            "990000",
        );
        assert!(m.package_interval_power(true).is_none()); // baseline
        write(
            &root.join("sys/class/powercap/intel-rapl:0/energy_uj"),
            "20000",
        ); // wrapped by +30000
        // dt here is sub-second; just assert a Some power came out
        assert!(m.package_interval_power(true).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn throttle_delta() {
        let root = std::env::temp_dir().join("nuc-fix-cpu-throttle");
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("sys/devices/system/cpu/cpu0/thermal_throttle");
        fs::create_dir_all(&dir).unwrap();
        write(&dir.join("core_throttle_count"), "100");
        write(&dir.join("package_throttle_count"), "0");
        let m = CpuMonitor::new(SourcePaths::fixture(&root), Topology::default());
        let (info, nc, np) = m.sample_throttle();
        assert!(info.supported);
        assert!(nc.is_none() && np.is_none()); // first sample
        write(&dir.join("core_throttle_count"), "104");
        write(&dir.join("package_throttle_count"), "2");
        let (_, nc, np) = m.sample_throttle();
        assert_eq!(nc, Some(4));
        assert_eq!(np, Some(2));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_reads_break_usage_energy_and_throttle_baselines() {
        let root = std::env::temp_dir().join("nuc-cpu-failed-read");
        let _ = fs::remove_dir_all(&root);
        let stat = root.join("proc/stat");
        let throttle =
            root.join("sys/devices/system/cpu/cpu0/thermal_throttle/core_throttle_count");
        let energy = root.join("sys/class/powercap/intel-rapl:0/energy_uj");
        write(&stat, "cpu 100 0 0 400 100 0 0 0\n");
        write(&throttle, "100");
        write(&energy, "1000000");
        let monitor = CpuMonitor::new(SourcePaths::fixture(&root), Topology::default());
        monitor.sample(true);
        for path in [&stat, &throttle, &energy] {
            fs::remove_file(path).unwrap();
        }
        monitor.sample(true);
        write(&stat, "cpu 200 0 0 500 100 0 0 0\n");
        write(&throttle, "105");
        write(&energy, "2000000");
        let recovered = monitor.sample(true);
        assert_eq!(recovered.usage_total, None);
        assert_eq!(recovered.interval_power_w, None);
        assert_eq!(recovered.new_core_throttles, None);
        fs::remove_dir_all(root).unwrap();
    }
}
