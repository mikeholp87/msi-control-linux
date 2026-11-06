//! Shared data models. Every reading carries its source so the UI can display
//! "Unavailable"/"Not supported" honestly instead of inventing values.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Classification used by the first-run capability report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    /// Present on the detected machine, readable/writable right now.
    Available,
    /// Kernel/driver support exists but requires authorization (polkit, CAP_*).
    RequiresAuthorization,
    /// Kernel support exists, but this configuration/hardware block doesn't expose it.
    SupportedUnavailable,
    /// Not supported by the detected hardware or driver.
    Unsupported,
    /// Not yet determined or needs verification.
    Unknown,
    /// Present but of uncertain reliability (e.g. state-encoded fan values).
    Uncertain,
}

/// Core class derived from kernel topology (never from CPU index alone).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CoreKind {
    /// Performance core (has thread sibling in this architecture, capacity cross-checked).
    Performance,
    /// Efficient core (no sibling thread).
    Efficient,
    /// Classification basis unavailable.
    Unknown,
}

impl std::fmt::Display for CoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreKind::Performance => write!(f, "Performance-core"),
            CoreKind::Efficient => write!(f, "Efficient-core"),
            CoreKind::Unknown => write!(f, "Unknown-type"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoreTopology {
    pub core_id: u32,
    pub threads: Vec<u32>,
    pub kind: CoreKind,
    pub capacity: Option<u64>,
}

/// Parsed CPU topology.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Topology {
    pub logical_cpus: u32,
    pub cores: Vec<CoreTopology>,
    pub classification_basis: String,
}

impl Topology {
    pub fn physical_cores(&self) -> u32 {
        self.cores.len() as u32
    }

    pub fn count(&self, kind: CoreKind) -> u32 {
        self.cores.iter().filter(|c| c.kind == kind).count() as u32
    }
}

/// How a hybrid classification was determined; reported verbatim in the UI.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SystemIdentity {
    pub vendor: String,
    pub product: String,
    pub board: String,
    pub bios_vendor: String,
    pub bios_version: String,
    pub bios_date: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// e.g. "Intel(R) Core(TM) 7 150U"
    pub model: String,
    pub vendor: String,
}

// ---------------------------------------------------------------- CPU

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThrottleInfo {
    /// Interface present at all (`/sys/.../thermal_throttle`).
    pub supported: bool,
    pub core_throttle_count: Option<u64>,
    pub core_total_time_ms: Option<u64>,
    pub core_max_time_ms: Option<u64>,
    pub package_throttle_count: Option<u64>,
    pub package_total_time_ms: Option<u64>,
    pub package_max_time_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CpuPolicy {
    pub id: String,
    pub cpus: Vec<u32>,
    pub f_min_khz: Option<u64>,
    pub f_max_khz: Option<u64>,
    pub f_cur_khz: Option<u64>,
    /// Requested scaling frequency, NOT a verified physical clock measurement.
    pub hw_pref_khz: Option<u64>,
}

/// Copy of the CPUFreq driver state at sample time.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CpuFreqState {
    pub running_driver: String,
    pub intel_pstate_mode: Option<String>,
    pub governor: Option<String>,
    pub available_governors: Vec<String>,
    pub epp: Option<String>,
    pub available_epps: Vec<String>,
    pub turbo_enabled: Option<bool>,
    pub policies: Vec<CpuPolicy>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CpuSnapshot {
    pub info: DeviceInfo,
    pub topology: Topology,
    /// One usage value per logical CPU, 0..100. None after a sampling gap.
    pub usage: Vec<Option<f64>>,
    pub usage_total: Option<f64>,
    pub freq: CpuFreqState,
    /// Measured package power (W), CPU time windows only; see RAPL note.
    pub package_power_w: Option<f64>,
    /// Interval avg of package power since previous sample (None on first/gap).
    pub interval_power_w: Option<f64>,
    /// Peak power observed over the monitoring session.
    pub peak_power_w: Option<f64>,
    pub throttle: ThrottleInfo,
    /// Deltas of throttle counters this sample vs. previous (None on first).
    pub new_core_throttles: Option<u64>,
    pub new_package_throttles: Option<u64>,
}

// ------------------------------------------------------------ THERMAL

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Temperature {
    /// Stable id: device path + channel, e.g. "coretemp.0/temp1".
    pub id: String,
    pub label: String,
    /// degrees Celsius; None if sensor read failed this sample.
    pub value_c: Option<f64>,
    /// Driver-reported limits when exposed (degrees C).
    pub warn_c: Option<f64>,
    pub crit_c: Option<f64>,
    pub max_c: Option<f64>,
    pub alarm: bool,
    /// Source chip/driver, e.g. "coretemp".
    pub source: String,
    /// Human note about source quality (e.g. acpi_fan uncertainty analogues).
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FanReading {
    pub id: String,
    pub label: String,
    pub rpm: Option<f64>,
    pub source: String,
    pub note: Option<String>,
    /// Driver-reported fault dip when the kernel exposes one (`fanN_alarm`).
    pub fault: Option<bool>,
}

impl FanReading {
    pub fn formatted_speed(&self) -> String {
        self.rpm
            .map(|value| {
                if self.source == "acpi_fan" {
                    format!("{value:.0} (ACPI-reported value)")
                } else {
                    format!("{value:.0} RPM")
                }
            })
            .unwrap_or_else(|| "Unavailable".into())
    }
}

// -------------------------------------------------------------- POWER

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaplZone {
    pub id: String,
    pub name: String,
    /// Cumulative energy in µJ (raw), or None if unreadable.
    pub energy_uj: Option<u64>,
    /// Counter wrap range (µJ) used for wrap detection.
    pub max_energy_range_uj: Option<u64>,
    /// Interval power derived from consecutive samples (W).
    pub power_w: Option<f64>,
    /// Constraint labels as exposed by the driver (read-only telemetry).
    pub constraints: Vec<RaplConstraint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaplConstraint {
    pub name: String,
    pub power_limit_uw: Option<u64>,
    pub max_power_uw: Option<u64>,
    pub time_window_us: Option<u64>,
    /// Whether the running user could actually change it (always false in GUI).
    pub writable: bool,
}

// --------------------------------------------------------------- GPU

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuEngine {
    pub name: String,
    /// utilization percent
    pub busy_pct: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GpuSnapshot {
    pub id: String,
    pub device: String,
    pub driver: String,
    pub render_node: Option<String>,
    /// driver-reported current frequency (MHz).
    pub freq_mhz: Option<u64>,
    pub act_freq_mhz: Option<u64>,
    pub max_freq_mhz: Option<u64>,
    pub rp0_freq_mhz: Option<u64>,
    /// GPU utilization overall 0..100 or None if unsupported/restricted.
    pub utilization_pct: Option<f64>,
    /// how utilization was obtained, or why it is unavailable.
    pub utilization_source: String,
    pub engines: Vec<GpuEngine>,
}

// ------------------------------------------------------------ MEMORY

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub total_kib: Option<u64>,
    pub used_kib: Option<u64>,
    pub available_kib: Option<u64>,
    pub cached_kib: Option<u64>,
    pub buffers_kib: Option<u64>,
    pub swap_total_kib: Option<u64>,
    pub swap_used_kib: Option<u64>,
    /// Memory pressure fraction (0..1 full), only when /proc/pressure exposes it.
    pub pressure: Option<f64>,
}

// ----------------------------------------------------------- STORAGE

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NvmeHealth {
    /// NVMe "percentage" estimate (0..100 gross use). NOT exact remaining life.
    pub percent_used: Option<u8>,
    pub avail_spare_pct: Option<u8>,
    pub spare_threshold_pct: Option<u8>,
    pub units_read: Option<u64>,
    pub units_written: Option<u64>,
    pub power_on_hours: Option<u64>,
    pub unsafe_shutdowns: Option<u64>,
    pub media_errors: Option<u64>,
    pub error_log_entries: Option<u64>,
    pub critical_warning: Option<u64>,
    /// UDisks2 controller warning labels; None when the property is unavailable.
    pub critical_warnings: Option<Vec<String>>,
    pub warn_temp_c: Option<f64>,
    pub crit_temp_c: Option<f64>,
    /// Which backend produced the data (e.g. "UDisks2").
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StorageDevice {
    pub node: String,
    pub model: Option<String>,
    pub size_bytes: Option<u64>,
    pub transport: String,
    /// Whether rotating (hard disk) per kernel queue info.
    pub rotational: bool,
    pub firmware: Option<String>,
    pub temp_c: Option<f64>,
    pub temp_crit_c: Option<f64>,
    /// Partition/file system summary is provided separately where readable.
    pub partitions: Vec<PartitionInfo>,
    pub nvme_health: Option<NvmeHealth>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub node: String,
    pub size_bytes: Option<u64>,
    /// mountpoint (first mounted one) if any
    pub mountpoint: Option<String>,
    /// filesystem claimed by kernel
    pub fstype: Option<String>,
}

// ----------------------------------------------------------- PROFILE

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PowerProfileState {
    pub backend: String,
    pub active: Option<String>,
    pub profiles: Vec<String>,
    pub degraded: bool,
    /// Optional extra cause when degraded (e.g. platform throttle driver).
    pub degraded_reason: Option<String>,
}

// ----------------------------------------------------------- ALERTS

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AlertKind {
    CpuTemp,
    NvmeTemp,
    FanFault,
    Throttle,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlertPrefs {
    pub cpu_temp_warm_c: Option<f64>,
    pub nvme_temp_warm_c: Option<f64>,
    pub fan_fault: bool,
    pub throttle_warn: bool,
    pub hysteresis_c: f64,
    pub above_samples: u32,
    pub cooldown_s: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertEvent {
    pub ts_ms: i64,
    pub kind: AlertKind,
    /// notice | warning | critical
    pub level: String,
    pub message: String,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
}

// --------------------------------------------------------- SNAPSHOT

/// A complete sample of everything readable at a point in time.
#[allow(dead_code)]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts_ms: i64,
    /// Wall ms between this sample and the previous sample. 0 on first.
    pub delta_ms: i64,
    /// false when a sampling gap (suspend etc.) invalidates delta-based metrics.
    pub deltas_valid: bool,
    /// Session running peak of measured package power (W).
    pub peak_power_w: Option<f64>,
    pub cpu: CpuSnapshot,
    pub gpu: Option<GpuSnapshot>,
    pub temperatures: Vec<Temperature>,
    pub fans: Vec<FanReading>,
    pub rapl: Vec<RaplZone>,
    pub memory: MemorySnapshot,
    pub storage: Vec<StorageDevice>,
    pub profile: Option<PowerProfileState>,
}

/// Default monitor heartbeat.
pub const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(2000);
