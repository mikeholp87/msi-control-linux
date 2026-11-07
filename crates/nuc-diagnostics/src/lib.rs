//! Read-only sanity checks: sensors, CPU/policy, GPU, power paths, storage
//! health, service conflicts. Produces a status report renderable as
//! Markdown or JSON.
//!
//! Diagnostics never writes any kernel state: profile changes are offered
//! only through the GUI/CLI layer on top of `nuc-power`'s verified D-Bus path.

use nuc_core::config::Config;
use nuc_core::{AppError, CoreKind, Snapshot};
use nuc_monitor::engine::Engine;
use nuc_monitor::fs::SourcePaths;
use nuc_monitor::sysinfo;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ------------------------------------------------------------ MODEL

/// Result discriminator for each check. `Unavailable` is used whenever a
/// backend did not answer — the report never invents a value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Normal as expected for this platform.
    Ok,
    /// Present anomalies worth watching (degraded, thresholds).
    Warning,
    /// Something is wrong and may affect operation.
    Fail,
    /// Contextual information (no action implied).
    Info,
    /// The check could not decide (backend absent).
    Unavailable,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warning => "warning",
            Status::Fail => "fail",
            Status::Info => "info",
            Status::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub category: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(id: &str, category: &str, status: Status, detail: impl Into<String>) -> Check {
        Check {
            id: id.into(),
            category: category.into(),
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagnosticReport {
    /// UTC seconds-since-epoch when the report was generated.
    pub generated_ms: i64,
    /// "live" or a fixture root label (tests, self-check runs).
    pub source: String,
    pub checks: Vec<Check>,
}

impl DiagnosticReport {
    pub fn status_count(&self, s: Status) -> usize {
        self.checks.iter().filter(|c| c.status == s).count()
    }

    /// Short one-line verdict for the CLI header row.
    pub fn verdict(&self) -> String {
        let f = self.status_count(Status::Fail);
        let w = self.status_count(Status::Warning);
        if f > 0 {
            format!("{f} failure(s), {w} warning(s)")
        } else if w > 0 {
            format!("{w} warning(s)")
        } else if self.status_count(Status::Unavailable) > 0 {
            format!(
                "limited checks: {} unavailable",
                self.status_count(Status::Unavailable)
            )
        } else if self.status_count(Status::Ok) == 0 {
            "no checks completed".into()
        } else {
            "all checks passed".into()
        }
    }

    pub fn export_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# NUC Control diagnostics\n\n");
        out.push_str(&format!(
            "- generated: {}\n- source: {}\n- verdict: {}\n\n",
            self.generated_ms,
            self.source,
            self.verdict()
        ));
        let mut category: Option<String> = None;
        for c in &self.checks {
            if category.as_deref() != Some(c.category.as_str()) {
                out.push_str(&format!(
                    "## {}\n\n| check | status | detail |\n|---|---|---|\n",
                    c.category
                ));
                category = Some(c.category.clone());
            }
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                c.id,
                c.status.as_str(),
                escape_md(&c.detail)
            ));
        }
        out.push('\n');
        out
    }
}

fn escape_md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', "<br>")
}

// ------------------------------------------------------------ RUN

/// Run all checks. `paths` selects the live system (`SourcePaths::system()`)
/// or a synthetic fixture root. `source_label` describes the scope.
pub fn run(
    paths: &SourcePaths,
    config: &Config,
    source_label: &str,
) -> Result<DiagnosticReport, AppError> {
    let mut checks = Vec::new();
    let mut engine = Engine::build(
        paths,
        &nuc_monitor::SamplerConfig {
            interval: std::time::Duration::from_secs(2),
            gpu_enabled: config.monitoring.gpu_enabled,
            record: false,
        },
    )?;
    let _ = engine.sample(); // establishes delta baselines (usage/power)
    std::thread::sleep(std::time::Duration::from_millis(700));
    let snap = engine.sample();

    // Profile state comes straight from the daemon, not the sampler.
    let mut snap = snap;
    let live = paths.sys == Path::new("/sys") && paths.proc == Path::new("/proc");
    if live && snap.profile.is_none() {
        snap.profile = nuc_power::ProfileClient::new()
            .ok()
            .and_then(|c| c.state().ok());
    }

    if live {
        system_checks(&mut checks);
        populate_storage_health(&mut checks, &mut snap, nuc_power::nvme_health);
    }
    thermal_checks(&mut checks, &snap, config);
    fan_checks(&mut checks, &snap);
    cpu_checks(&mut checks, &snap);
    gpu_checks(&mut checks, &snap);
    rapl_checks(&mut checks, &snap);
    storage_checks(&mut checks, &snap);
    memory_checks(&mut checks, &snap);
    profile_checks(&mut checks, &snap);
    if live {
        service_checks(&mut checks);
        history_checks(&mut checks, &nuc_core::config::data_dir());
    }

    Ok(DiagnosticReport {
        generated_ms: now_ms(),
        source: source_label.to_string(),
        checks,
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn system_checks(checks: &mut Vec<Check>) {
    let info = sysinfo::discover();
    let id = &info.identity;
    checks.push(Check::new(
        "system",
        "System",
        Status::Info,
        format!(
            "kernel {}; {}; DMI {} / {}; BIOS {} {} ({})",
            info.kernel.trim(),
            if info.distro.is_empty() {
                "unknown distro"
            } else {
                info.distro.trim()
            },
            if id.vendor.is_empty() {
                "vendor n/a"
            } else {
                id.vendor.trim()
            },
            if id.product.is_empty() {
                "product n/a"
            } else {
                id.product.trim()
            },
            if id.bios_vendor.is_empty() {
                "BIOS n/a"
            } else {
                id.bios_vendor.trim()
            },
            id.bios_version.trim(),
            id.bios_date.trim()
        ),
    ));
    checks.push(Check::new(
        "session",
        "System",
        Status::Info,
        format!(
            "session type {}; desktop {}; session D-Bus address {}",
            if info.session_type.is_empty() {
                "unknown"
            } else {
                &info.session_type
            },
            if info.desktop_env.is_empty() {
                "unknown"
            } else {
                &info.desktop_env
            },
            if info.dbus_accessible {
                "present (connectivity not verified)"
            } else {
                "unreachable"
            }
        ),
    ));
}

fn thermal_checks(checks: &mut Vec<Check>, snap: &Snapshot, config: &Config) {
    let core_temps: Vec<_> = snap
        .temperatures
        .iter()
        .filter(|t| t.source.contains("coretemp") || t.id.contains("coretemp"))
        .collect();
    if core_temps.is_empty() {
        checks.push(Check::new(
            "cpu-temperature",
            "Thermal",
            Status::Unavailable,
            "no coretemp readings found — CPU temperature unavailable",
        ));
    } else {
        let hottest = core_temps
            .iter()
            .filter_map(|t| t.value_c)
            .fold(None::<f64>, |a, v| a.map_or(Some(v), |a| Some(a.max(v))));
        match hottest {
            Some(c) => {
                let status = match config.alerts.cpu_temp_warm_c {
                    Some(w) if c >= w => Status::Warning,
                    _ => Status::Ok,
                };
                checks.push(Check::new(
                    "cpu-temperature",
                    "Thermal",
                    status,
                    format!(
                        "hottest core {c:.1} °C across {} coretemp sensor(s)",
                        core_temps.len()
                    ),
                ));
            }
            None => checks.push(Check::new(
                "cpu-temperature",
                "Thermal",
                Status::Unavailable,
                "coretemp sensors present but returned no values this sample",
            )),
        }
    }
    for t in &snap.temperatures {
        if t.alarm {
            checks.push(Check::new(
                "sensor-alarm",
                "Thermal",
                Status::Warning,
                format!("{} reports hardware alarm", t.id),
            ));
        }
    }
}

fn fan_checks(checks: &mut Vec<Check>, snap: &Snapshot) {
    if snap.fans.is_empty() {
        checks.push(Check::new(
            "fan-sources",
            "Cooling",
            Status::Unavailable,
            "no fan sensor present (uncertainty: acpi_fan exposes fan1_input with ".to_string()
                + "state-encoded values on some boards; manual PWM control does not exist)",
        ));
        return;
    }
    for f in &snap.fans {
        if f.fault == Some(true) {
            checks.push(Check::new(
                "fan-fault",
                "Cooling",
                Status::Fail,
                format!("{} reports a fault flag", f.id),
            ));
        } else {
            let rpm = f.formatted_speed();
            checks.push(Check::new(
                "fan-speed",
                "Cooling",
                if f.rpm.is_some() {
                    Status::Ok
                } else {
                    Status::Unavailable
                },
                format!(
                    "{} reads {rpm}{}",
                    f.id,
                    f.note
                        .as_ref()
                        .map(|n| format!(" — {n}"))
                        .unwrap_or_default()
                ),
            ));
        }
    }
}

fn cpu_checks(checks: &mut Vec<Check>, snap: &Snapshot) {
    let topo = &snap.cpu.topology;
    checks.push(Check::new(
        "cpu-topology",
        "CPU",
        Status::Info,
        format!(
            "{} physical cores ({} P-cores, {} E-cores; {} logical) — basis: {}",
            topo.physical_cores(),
            topo.count(CoreKind::Performance),
            topo.count(CoreKind::Efficient),
            topo.logical_cpus,
            topo.classification_basis
        ),
    ));
    if snap.cpu.new_core_throttles.unwrap_or(0) > 0
        || snap.cpu.new_package_throttles.unwrap_or(0) > 0
    {
        checks.push(Check::new(
            "throttle-delta",
            "CPU",
            Status::Warning,
            format!(
                "throttle counters increased this sample (core +{:?}, package +{:?})",
                snap.cpu.new_core_throttles, snap.cpu.new_package_throttles
            ),
        ));
    } else {
        let t = &snap.cpu.throttle;
        let detail = if t.supported {
            format!(
                "cumulative counters: core {}, package {}; no new events",
                t.core_throttle_count
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".into()),
                t.package_throttle_count
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".into())
            )
        } else {
            "thermal throttle interface not present (no counter available)".into()
        };
        let status =
            if snap.cpu.new_core_throttles.is_some() || snap.cpu.new_package_throttles.is_some() {
                Status::Ok
            } else {
                Status::Unavailable
            };
        checks.push(Check::new("throttle-counters", "CPU", status, detail));
    }
    match snap.cpu.usage_total {
        Some(u) if u > 99.5 => checks.push(Check::new(
            "cpu-utilization",
            "CPU",
            Status::Warning,
            format!("CPU at {u:.1}% during check"),
        )),
        Some(u) => checks.push(Check::new(
            "cpu-utilization",
            "CPU",
            Status::Ok,
            format!("CPU at {u:.1}% during check"),
        )),
        None => checks.push(Check::new(
            "cpu-utilization",
            "CPU",
            Status::Unavailable,
            "usage counters not readable this sample",
        )),
    }
}

fn gpu_checks(checks: &mut Vec<Check>, snap: &Snapshot) {
    match &snap.gpu {
        Some(g) => {
            checks.push(Check::new(
                "gpu-driver",
                "GPU",
                Status::Info,
                format!(
                    "{} ({}) — freq {}/{} MHz (act/max)",
                    g.id,
                    g.driver,
                    g.act_freq_mhz
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "n/a".into()),
                    g.max_freq_mhz
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "n/a".into()),
                ),
            ));
            checks.push(Check::new(
                "gpu-utilization",
                "GPU",
                if g.utilization_pct.is_some() {
                    Status::Ok
                } else {
                    Status::Unavailable
                },
                g.utilization_source.clone(),
            ));
        }
        None => checks.push(Check::new(
            "gpu",
            "GPU",
            Status::Unavailable,
            "no GPU snapshot (gpu disabled in config or none found)",
        )),
    }
}

fn rapl_checks(checks: &mut Vec<Check>, snap: &Snapshot) {
    if snap.rapl.is_empty() {
        checks.push(Check::new(
            "rapl",
            "Power",
            Status::Unavailable,
            "no RAPL zones readable (power telemetry unavailable)",
        ));
    } else {
        let zones: Vec<String> = snap.rapl.iter().map(|z| z.id.clone()).collect();
        let zones = zones.join(", ");
        let power_note = snap
            .cpu
            .interval_power_w
            .map(|w| format!(" — package power {w:.1} W this interval"))
            .unwrap_or_default();
        checks.push(Check::new(
            "rapl-read",
            "Power",
            Status::Ok,
            format!("{} zone(s): [{zones}]{power_note}", snap.rapl.len()),
        ));
    }
}

fn storage_checks(checks: &mut Vec<Check>, snap: &Snapshot) {
    if snap.storage.is_empty() {
        checks.push(Check::new(
            "storage",
            "Storage",
            Status::Unavailable,
            "no storage devices found",
        ));
        return;
    }
    for d in &snap.storage {
        checks.push(Check::new(
            "storage-device",
            "Storage",
            Status::Info,
            format!(
                "{}: {} ({} {})",
                d.node,
                d.model.clone().unwrap_or_else(|| "unknown".into()),
                d.size_bytes
                    .map(nuc_core::units::fmt_bytes)
                    .unwrap_or_else(|| "size unavailable".into()),
                d.transport
            ),
        ));
        if let Some(h) = &d.nvme_health {
            if let Some(pu) = h.percent_used {
                let status = if pu >= 85 {
                    Status::Warning
                } else {
                    Status::Ok
                };
                checks.push(Check::new(
                    "nvme-wear",
                    "Storage",
                    status,
                    format!("{}: NVMe percent_used {pu}%", d.node),
                ));
            }
            match (h.avail_spare_pct, h.spare_threshold_pct) {
                (Some(sp), Some(th)) if sp <= th => checks.push(Check::new(
                    "nvme-spare",
                    "Storage",
                    Status::Fail,
                    format!("{}: spare capacity {sp}% at/below threshold {th}%", d.node),
                )),
                (Some(sp), Some(th)) => checks.push(Check::new(
                    "nvme-spare",
                    "Storage",
                    Status::Ok,
                    format!("{}: spare capacity {sp}% (threshold {th}%)", d.node),
                )),
                _ => {}
            }
            if h.media_errors.unwrap_or(0) > 0 {
                checks.push(Check::new(
                    "nvme-media-errors",
                    "Storage",
                    Status::Warning,
                    format!("{}: media errors {} (>0)", d.node, h.media_errors.unwrap()),
                ));
            }
            if h.critical_warning.unwrap_or(0) != 0
                || h.critical_warnings
                    .as_ref()
                    .is_some_and(|warnings| !warnings.is_empty())
            {
                checks.push(Check::new(
                    "nvme-critical",
                    "Storage",
                    Status::Fail,
                    format!(
                        "{}: SMART critical warning: flags {:?}, controller {:?}",
                        d.node, h.critical_warning, h.critical_warnings
                    ),
                ));
            } else if h.critical_warning.is_none() && h.critical_warnings.is_none() {
                checks.push(Check::new(
                    "nvme-critical",
                    "Storage",
                    Status::Unavailable,
                    format!("{}: controller critical-warning status unavailable", d.node),
                ));
            }
        } else if d.transport == "NVMe"
            && !checks
                .iter()
                .any(|c| c.id == "nvme-health" && c.detail.starts_with(&format!("{}:", d.node)))
        {
            checks.push(Check::new(
                "nvme-health",
                "Storage",
                Status::Unavailable,
                format!("{}: SMART health unavailable", d.node),
            ));
        }
    }
}

fn populate_storage_health(
    checks: &mut Vec<Check>,
    snap: &mut Snapshot,
    mut fetch: impl FnMut(&str) -> nuc_core::Result<nuc_core::NvmeHealth>,
) {
    for disk in snap.storage.iter_mut().filter(|d| d.transport == "NVMe") {
        match fetch(&disk.node) {
            Ok(health) => disk.nvme_health = Some(health),
            Err(error) => checks.push(Check::new(
                "nvme-health",
                "Storage",
                Status::Unavailable,
                format!("{}: {error}", disk.node),
            )),
        }
    }
}

fn memory_checks(checks: &mut Vec<Check>, snap: &Snapshot) {
    let pct = match (snap.memory.used_kib, snap.memory.total_kib) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 * 100.0 / t as f64),
        _ => None,
    };
    let (status, body) = match pct {
        Some(p) if p > 95.0 => (
            Status::Warning,
            format!("RAM usage {p:.1}% — near capacity"),
        ),
        Some(p) => (Status::Ok, format!("RAM usage {p:.1}%")),
        None => (Status::Unavailable, "memory info not readable".into()),
    };
    checks.push(Check::new("memory", "Memory", status, body));
}

fn profile_checks(checks: &mut Vec<Check>, snap: &Snapshot) {
    match &snap.profile {
        Some(p) => {
            let status = if p.degraded {
                Status::Warning
            } else {
                Status::Ok
            };
            let reason = p
                .degraded_reason
                .clone()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default();
            checks.push(Check::new(
                "power-profile",
                "Power policy",
                status,
                format!(
                    "active: {} (available: {:?}){reason}",
                    p.active.clone().unwrap_or_else(|| "unavailable".into()),
                    p.profiles
                ),
            ));
        }
        None => checks.push(Check::new(
            "power-profile",
            "Power policy",
            Status::Unavailable,
            "power-profiles-daemon state unavailable (not running or restricted)",
        )),
    }
}

fn service_checks(checks: &mut Vec<Check>) {
    let conflicts = nuc_power::conflict_scan();
    if conflicts.is_empty() {
        checks.push(Check::new(
            "services",
            "Services",
            Status::Unavailable,
            "unit states could not be queried",
        ));
    }
    for (name, state) in conflicts {
        let (status, detail) = if state.starts_with("active") {
            if name.contains("power-profiles-daemon") || name.contains("UPower") {
                (Status::Ok, format!("{name}: {state}"))
            } else {
                (Status::Warning, format!("{name}: {state}"))
            }
        } else if ["unavailable", "unreachable", "unqueried", "unknown"]
            .iter()
            .any(|s| state.contains(s))
        {
            (Status::Unavailable, format!("{name}: {state}"))
        } else {
            (Status::Info, format!("{name}: {state}"))
        };
        checks.push(Check::new("services", "Services", status, detail));
    }
}

/// Appends read-only checks of an existing history database.
pub fn history_checks(checks: &mut Vec<Check>, dir: &Path) {
    match nuc_history::HistoryStore::open_read_only_in_dir(dir) {
        Ok(store) => match store.row_counts() {
            Ok((s, m, e)) => {
                let mib = store
                    .size_bytes()
                    .map(|b| b as f64 / 1_048_576.0)
                    .unwrap_or(0.0);
                checks.push(Check::new(
                    "history-db",
                    "History",
                    Status::Ok,
                    format!(
                        "DB at {} ({mib:.1} MiB; samples {s}, minutes {m}, events {e})",
                        dir.display()
                    ),
                ));
            }
            Err(e) => checks.push(Check::new(
                "history-db",
                "History",
                Status::Warning,
                format!("DB opened but counters failed: {e}"),
            )),
        },
        Err(e) => checks.push(Check::new(
            "history-db",
            "History",
            Status::Unavailable,
            format!("{e}"),
        )),
    }
}

// ------------------------------------------------------------ TESTS

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> DiagnosticReport {
        DiagnosticReport {
            generated_ms: 1_700_000_000_000,
            source: "fixture".into(),
            checks: vec![
                Check::new("system", "System", Status::Info, "kernel test"),
                Check::new("cpu-temperature", "Thermal", Status::Ok, "hottest 42.0 °C"),
                Check::new("fan-speed", "Cooling", Status::Ok, "fan1 32 RPM"),
                Check::new("nvme-wear", "Storage", Status::Warning, "wear 90% | high"),
                Check::new(
                    "nvme-critical",
                    "Storage",
                    Status::Fail,
                    "critical_warning=0x4",
                ),
                Check::new("gpu-utilization", "GPU", Status::Unavailable, "not running"),
            ],
        }
    }

    #[test]
    fn verdict_counts() {
        let r = sample_report();
        assert_eq!(r.status_count(Status::Fail), 1);
        assert_eq!(r.status_count(Status::Warning), 1);
        assert_eq!(r.status_count(Status::Ok), 2);
        assert_eq!(r.status_count(Status::Unavailable), 1);
        assert_eq!(r.verdict(), "1 failure(s), 1 warning(s)");
    }

    #[test]
    fn markdown_groups_by_category() {
        let md = sample_report().export_markdown();
        assert!(md.contains("# NUC Control diagnostics"));
        assert!(md.contains("## System\n"));
        assert!(md.contains("## Thermal\n"));
        assert!(md.contains("## Cooling\n"));
        assert!(md.contains("## Storage\n"));
        assert!(md.contains("| `nvme-wear` | warning | wear 90% \\| high |"));
        // categories appear once each
        assert_eq!(md.matches("## Thermal").count(), 1);
        assert_eq!(md.matches("## Storage").count(), 1);
    }

    #[test]
    fn markdown_escapes_pipes_and_newlines() {
        let r = DiagnosticReport {
            generated_ms: 0,
            source: "fixture".into(),
            checks: vec![Check::new("t", "C", Status::Ok, "a|b\nc")],
        };
        let md = r.export_markdown();
        assert!(md.contains("a\\|b<br>c"));
    }

    #[test]
    fn status_serde_snake_case() {
        let json = serde_json::to_string(&Status::Warning).unwrap();
        assert_eq!(json, r#""warning""#);
        let back: Status = serde_json::from_str(r#""unavailable""#).unwrap();
        assert_eq!(back, Status::Unavailable);
    }

    #[test]
    fn unavailable_checks_are_not_a_clean_bill_of_health() {
        let mut report = DiagnosticReport {
            generated_ms: 0,
            source: "fixture".into(),
            checks: vec![],
        };
        fan_checks(
            &mut report.checks,
            &Snapshot {
                fans: vec![nuc_core::FanReading::default()],
                ..Default::default()
            },
        );
        cpu_checks(&mut report.checks, &Snapshot::default());
        assert!(
            report
                .checks
                .iter()
                .filter(|c| c.id == "fan-speed" || c.id == "throttle-counters")
                .all(|c| c.status == Status::Unavailable)
        );
        assert!(report.verdict().contains("unavailable"));
        assert!(!report.verdict().contains("all checks passed"));
    }

    #[test]
    fn smart_faults_are_checked_and_backend_failure_is_unavailable() {
        let mut snap = Snapshot {
            storage: vec![nuc_core::StorageDevice {
                node: "/dev/nvme0n1".into(),
                transport: "NVMe".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut checks = Vec::new();
        populate_storage_health(&mut checks, &mut snap, |_| {
            Ok(nuc_core::NvmeHealth {
                critical_warning: Some(4),
                ..Default::default()
            })
        });
        storage_checks(&mut checks, &snap);
        assert!(
            checks
                .iter()
                .any(|c| c.id == "nvme-critical" && c.status == Status::Fail)
        );
        checks.clear();
        snap.storage[0].nvme_health = Some(nuc_core::NvmeHealth {
            critical_warnings: Some(vec!["degraded".into()]),
            ..Default::default()
        });
        storage_checks(&mut checks, &snap);
        assert!(
            checks
                .iter()
                .any(|c| c.id == "nvme-critical" && c.status == Status::Fail)
        );
        checks.clear();
        snap.storage[0].nvme_health = None;
        populate_storage_health(&mut checks, &mut snap, |_| {
            Err(AppError::Other("permission denied".into()))
        });
        storage_checks(&mut checks, &snap);
        assert_eq!(
            checks
                .iter()
                .filter(|c| c.id == "nvme-health" && c.status == Status::Unavailable)
                .count(),
            1
        );
    }

    #[test]
    fn missing_history_is_not_created_by_diagnostics() {
        let dir =
            std::env::temp_dir().join(format!("nuc-diagnostics-readonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut checks = Vec::new();
        history_checks(&mut checks, &dir);
        assert!(!dir.exists());
        assert_eq!(checks[0].status, Status::Unavailable);
    }
}
