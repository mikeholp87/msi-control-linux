//! Intel integrated GPU discovery and monitoring.
//!
//! Frequency comes from documented DRM sysfs (`gt_cur_freq_mhz` et al.).
//! Utilization requires the `intel_gpu_top` PMU interface, which needs
//! `CAP_PERFMON`; when that is denied the app records **why** and shows
//! "Requires authorization" rather than faking data.
//!
//! Lifecycle: at most one `intel_gpu_top` child per app, started lazily and
//! killed on drop. A dedicated reader thread parses JSON output into an
//! atomic shared state so the sampler thread never blocks on the pipe.

use crate::fs::{SourcePaths, read_string_opt};
use nuc_core::{GpuEngine, GpuSnapshot, MutexExt};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, Read};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Absolute locations only: the app never resolves helpers through `$PATH`.
const INTEL_GPU_TOP_PATHS: [&str; 2] = ["/usr/bin/intel_gpu_top", "/usr/local/bin/intel_gpu_top"];
/// Cap on stderr bytes parsed; the rest is drained so the child never blocks.
const STDERR_PARSE_LIMIT: u64 = 1 << 20;
const PERFMON_NOTE: &str = "requires CAP_PERFMON (kernel refuses unprivileged access)";

pub struct GpuMonitor {
    paths: SourcePaths,
    card: Option<std::path::PathBuf>,
    driver: Option<String>,
    /// Optional background sample pipe.
    collector: Mutex<Option<Collector>>,
    interval_ms: u64,
    start_error: Option<String>,
}

/// Shared state written by the reader thread.
#[derive(Default, Clone)]
struct UtilSample {
    busy: Option<f64>,
    engines: Vec<GpuEngine>,
    updated_at: Option<std::time::Instant>,
}

struct Collector {
    child: Child,
    latest: Arc<Mutex<UtilSample>>,
    readers: Vec<JoinHandle<()>>,
    /// Mutable note surfaced by samples (permission reasons etc.).
    note: Arc<Mutex<Option<String>>>,
}

impl Drop for Collector {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for h in self.readers.drain(..) {
            let _ = h.join();
        }
    }
}

impl GpuMonitor {
    pub fn new(paths: SourcePaths, sample_interval_ms: u64) -> Self {
        GpuMonitor {
            paths,
            card: None,
            driver: None,
            collector: Mutex::new(None),
            interval_ms: sample_interval_ms.max(200),
            start_error: None,
        }
    }

    pub fn discover(&mut self) {
        if self.card.is_some() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.paths.class_drm) {
            let mut cards: Vec<std::path::PathBuf> = entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.starts_with("card")
                        .then(|| name[4..].parse::<u32>().ok())
                        .flatten()
                        .map(|_| e.path())
                })
                .collect();
            cards.sort();
            for path in cards {
                let dev = path.join("device");
                let driver = std::fs::read_link(dev.join("driver"))
                    .ok()
                    .and_then(|l| l.file_name().map(|f| f.to_string_lossy().into_owned()));
                if driver.as_deref().is_some_and(|d| d == "i915" || d == "xe") {
                    self.card = Some(path);
                    self.driver = driver;
                    return;
                }
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.card.is_some()
    }

    pub(crate) fn set_interval_ms(&mut self, interval_ms: u64) {
        let interval_ms = interval_ms.max(200);
        if self.interval_ms != interval_ms {
            self.stop_utilization();
            self.interval_ms = interval_ms;
        }
    }

    /// Render node matching this card's PCI device (renderDXXX sits beside
    /// the card in /sys/class/drm with the same underlying device).
    fn render_node(&self, card: &std::path::Path) -> Option<String> {
        let dev = std::fs::canonicalize(card.join("device")).ok()?;
        if let Ok(entries) = std::fs::read_dir(&self.paths.class_drm) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("renderD")
                    && let Ok(link) = std::fs::canonicalize(e.path().join("device"))
                    && link == dev
                {
                    return Some(name);
                }
            }
        }
        None
    }

    /// Driver-level status from sysfs only (no child process involved).
    pub fn sample(&self) -> Option<GpuSnapshot> {
        let card = self.card.clone()?;
        let cur = read_card_uint(&card, "gt_cur_freq_mhz");
        let act = read_card_uint(&card, "gt_act_freq_mhz");
        let max = read_card_uint(&card, "gt_max_freq_mhz");
        let rp0 = read_card_uint(&card, "gt_RP0_freq_mhz");
        let mut snap = GpuSnapshot {
            id: card.file_name().unwrap().to_string_lossy().into_owned(),
            device: std::fs::read_link(card.join("device"))
                .map(|l| l.to_string_lossy().into_owned())
                .unwrap_or_default(),
            driver: self.driver.clone().unwrap_or_default(),
            render_node: self.render_node(&card),
            freq_mhz: cur,
            act_freq_mhz: act,
            max_freq_mhz: max,
            rp0_freq_mhz: rp0,
            utilization_pct: None,
            utilization_source: String::new(),
            engines: Vec::new(),
        };
        let mut guard = self.collector.lock().ok()?;
        match guard.as_mut() {
            Some(c) => {
                let s = c.latest.lock_or_recover().clone();
                let running = c.child.try_wait().ok().flatten().is_none();
                let fresh = s.updated_at.is_some_and(|t| {
                    t.elapsed().as_millis()
                        <= u128::from(self.interval_ms.saturating_mul(3).max(5000))
                });
                if running && fresh {
                    snap.utilization_pct = s.busy;
                    snap.engines = s.engines;
                }
                snap.utilization_source = c
                    .note
                    .lock()
                    .map(|n| {
                        n.clone().unwrap_or_else(|| {
                            if running && fresh {
                                "intel_gpu_top (maximum engine busy %)"
                            } else {
                                "intel_gpu_top unavailable (waiting, stale or exited)"
                            }
                            .into()
                        })
                    })
                    .unwrap_or_else(|_| "intel_gpu_top".into());
            }
            None => {
                snap.utilization_source = self
                    .start_error
                    .clone()
                    .unwrap_or_else(|| "not running".into());
            }
        }
        Some(snap)
    }

    /// Start the intel_gpu_top collector; error out with a clear reason when
    /// unavailable (missing tool, spawn failure). PMU permission failures are
    /// detected from stderr and retained as `note`.
    pub fn start_utilization(&mut self) -> Result<(), nuc_core::AppError> {
        self.discover();
        if self.card.is_none() {
            return Err(nuc_core::AppError::Unsupported(
                "no Intel GPU detected".into(),
            ));
        }
        if self.collector.lock_or_recover().is_some() {
            return Ok(()); // already running; never spawn duplicates
        }
        let Some(tool) = INTEL_GPU_TOP_PATHS
            .iter()
            .map(std::path::Path::new)
            .find(|p| p.is_file())
        else {
            let reason = "intel_gpu_top (intel-gpu-tools) is not installed".to_string();
            self.start_error = Some(reason.clone());
            return Err(nuc_core::AppError::Unsupported(reason));
        };
        let child = Command::new(tool)
            .args(["-J", "-s", &self.interval_ms.to_string(), "-o", "-"])
            .args([
                "-d",
                &format!(
                    "drm:{}",
                    self.paths
                        .dev
                        .join("dri")
                        .join(self.card.as_ref().unwrap().file_name().unwrap())
                        .display()
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                let reason = if e.kind() == std::io::ErrorKind::NotFound {
                    "intel_gpu_top (intel-gpu-tools) is not installed".to_string()
                } else {
                    format!("failed to start intel_gpu_top: {e}")
                };
                self.start_error = Some(reason.clone());
                return Err(nuc_core::AppError::Unsupported(reason));
            }
        };
        let latest = Arc::new(Mutex::new(UtilSample::default()));
        let note = Arc::new(Mutex::new(None::<String>));
        let mut collector = Collector {
            child,
            latest: latest.clone(),
            readers: Vec::new(),
            note: note.clone(),
        };
        let stdout: ChildStdout = collector
            .child
            .stdout
            .take()
            .ok_or_else(|| nuc_core::AppError::Other("no stdout pipe".into()))?;
        let latest_r = latest.clone();
        let note_r = note.clone();
        let stderr = collector.child.stderr.take();
        let errors = std::thread::Builder::new()
            .name("intel_gpu_top-stderr".into())
            .spawn(move || {
                if let Some(mut stderr) = stderr {
                    for line in std::io::BufReader::new((&mut stderr).take(STDERR_PARSE_LIMIT))
                        .lines()
                        .map_while(Result::ok)
                    {
                        let reason = if line.contains("Permission denied")
                            || line.contains("Operation not permitted")
                        {
                            PERFMON_NOTE.to_string()
                        } else {
                            format!(
                                "intel_gpu_top stderr: {}",
                                line.chars().take(512).collect::<String>()
                            )
                        };
                        let mut note = note_r.lock_or_recover();
                        // A permission diagnosis is the actionable one; keep it.
                        if note.as_deref() != Some(PERFMON_NOTE) {
                            *note = Some(reason);
                        }
                    }
                    let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                }
            })
            .map_err(|e| nuc_core::AppError::Other(e.to_string()))?;
        collector.readers.push(errors);
        let note_r = note.clone();
        let reader = std::thread::Builder::new()
            .name("intel_gpu_top-json".into())
            .spawn(move || {
                if let Err(error) = read_gpu_samples(std::io::BufReader::new(stdout), |sample| {
                    *latest_r.lock_or_recover() = sample;
                }) {
                    *note_r.lock_or_recover() = Some(format!("intel_gpu_top output: {error}"));
                }
            })
            .map_err(|e| nuc_core::AppError::Other(e.to_string()))?;
        collector.readers.push(reader);
        *self.collector.lock_or_recover() = Some(collector);
        Ok(())
    }

    pub fn stop_utilization(&self) {
        *self.collector.lock_or_recover() = None; // Drop kills child & joins reader
    }
}

impl Drop for GpuMonitor {
    fn drop(&mut self) {
        self.stop_utilization();
    }
}

fn read_card_uint(card: &std::path::Path, attr: &str) -> Option<u64> {
    read_string_opt(&card.join(attr))?.trim().parse().ok()
}

/// Parse one `intel_gpu_top -J` JSON sample line into engine utilization.
fn parse_gpu_sample(v: &Value) -> Option<UtilSample> {
    let engines_obj = v.get("engines")?.as_object()?;
    let engines: Vec<GpuEngine> = engines_obj
        .iter()
        .map(|(name, data)| GpuEngine {
            name: name.clone(),
            busy_pct: data
                .get("busy")
                .and_then(|b| b.as_f64())
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v)),
        })
        .collect();
    if engines.is_empty() {
        return None;
    }
    // Overall utilization: busy counters of engine classes are not additive in
    // a strictly physical sense, so show the max class value; the UI lists
    // per-engine numbers separately and labels the aggregation honestly.
    let busy = engines
        .iter()
        .filter_map(|e| e.busy_pct)
        .fold(None::<f64>, |acc, b| Some(acc.unwrap_or(0.0).max(b)));
    Some(UtilSample {
        busy,
        engines,
        updated_at: Some(std::time::Instant::now()),
    })
}

/// The tool emits an open array of multiline objects, not JSON per line.
fn read_gpu_samples(
    mut reader: impl BufRead,
    mut receive: impl FnMut(UtilSample),
) -> std::io::Result<()> {
    loop {
        loop {
            match reader.fill_buf()?.first().copied() {
                None => return Ok(()),
                Some(b'{') => break,
                Some(b) if b.is_ascii_whitespace() || b"[,]".contains(&b) => reader.consume(1),
                _ => return Err(std::io::Error::other("expected a JSON sample object")),
            }
        }
        let value = Value::deserialize(&mut serde_json::Deserializer::from_reader(&mut reader))?;
        if let Some(sample) = parse_gpu_sample(&value) {
            receive(sample);
        }
    }
}

/// Card discovery entry listing (name, driver)
pub fn discover_cards(paths: &SourcePaths) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&paths.class_drm) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix("card")
                && rest.parse::<u32>().is_ok()
            {
                let driver = std::fs::read_link(e.path().join("device/driver"))
                    .ok()
                    .and_then(|l| l.file_name().map(|f| f.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                out.push((name, driver));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_multiline_samples_from_an_unfinished_array() {
        let input = "[\n{\n\"engines\": {\"Render/3D\": {\"busy\": 32.5}}\n},\n{\"engines\":{\"Video\":{\"busy\":75.0}}}";
        let mut values = Vec::new();
        read_gpu_samples(input.as_bytes(), |s| values.push(s.busy)).unwrap();
        assert_eq!(values, [Some(32.5), Some(75.0)]);
    }

    #[test]
    fn invalid_engine_measurements_stay_unavailable() {
        let sample = parse_gpu_sample(
            &serde_json::json!({"engines":{"Render":{"busy":-1},"Video":{"busy":101}}}),
        )
        .unwrap();
        assert_eq!(sample.busy, None);
    }

    #[test]
    fn interval_change_updates_collector_cadence_and_freshness() {
        let mut gpu = GpuMonitor::new(
            SourcePaths::fixture(std::path::Path::new("/tmp/nuc-gpu-interval")),
            60_000,
        );
        gpu.set_interval_ms(1000);
        assert_eq!(gpu.interval_ms, 1000);
        assert!(gpu.collector.lock().unwrap().is_none());
    }

    #[test]
    fn dropping_collector_reaps_its_child() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let collector = Collector {
            child,
            readers: vec![],
            latest: Arc::new(Mutex::new(UtilSample::default())),
            note: Arc::new(Mutex::new(None)),
        };
        drop(collector);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}
