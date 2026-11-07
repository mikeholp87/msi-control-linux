//! Sampling engine: one background thread reads all providers on a fixed
//! interval, publishes the newest snapshot and evaluates alerts. File reads
//! happen only on the sampler thread, so the GTK main loop never blocks on
//! sysfs. A suspend/stall gap longer than 4× the interval invalidates every
//! delta-based value (utilization, interval power) for that sample.

use crate::alerts::AlertEngine;
use crate::cpu::CpuMonitor;
use crate::fan::FanMonitor;
use crate::fs::SourcePaths;
use crate::gpu::GpuMonitor;
use crate::memory::MemoryMonitor;
use crate::power::RaplMonitor;
use crate::storage::StorageMonitor;
use crate::thermal::ThermalMonitor;
use crate::topologyparse;
use nuc_core::{AlertEvent, AlertPrefs, Snapshot};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct SamplerConfig {
    pub interval: Duration,
    pub gpu_enabled: bool,
    pub record: bool,
}

enum Command {
    AdjustInterval(Duration),
    SetGpuEnabled(bool),
    Stop,
}

/// Handle from the GUI/CLI thread: read the newest snapshot, tune the
/// sampler, or shut it down (no orphan threads or intel_gpu_top children).
#[derive(Clone)]
pub struct SampleHandle {
    tx: Sender<Command>,
    latest: Arc<Mutex<Option<Snapshot>>>,
}

impl SampleHandle {
    pub fn latest(&self) -> Option<Snapshot> {
        self.latest.lock().ok()?.clone()
    }

    pub fn set_interval(&self, interval: Duration) {
        if interval >= Duration::from_millis(500) {
            let _ = self.tx.send(Command::AdjustInterval(interval));
        }
    }

    /// Stop the sampler thread.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn set_gpu_enabled(&self, enabled: bool) {
        let _ = self.tx.send(Command::SetGpuEnabled(enabled));
    }
}

pub struct SamplerControls {
    pub alerts: Receiver<AlertEvent>,
    /// Closes/finishes when the sampler thread exits.
    pub done: Receiver<()>,
}

/// Start the single sampling thread. `on_snapshot` (optional) is invoked after
/// every sample — the history recorder hooks in here.
#[allow(clippy::type_complexity)]
pub fn start_sampler(
    paths: SourcePaths,
    config: SamplerConfig,
    alert_prefs: AlertPrefs,
    mut on_snapshot: Option<Box<dyn FnMut(&mut Snapshot) + Send>>,
) -> Result<(SampleHandle, SamplerControls), nuc_core::AppError> {
    let mut engine = Engine::build(&paths, &config)?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (alert_tx, alert_rx) = mpsc::channel::<AlertEvent>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let latest = Arc::new(Mutex::new(None::<Snapshot>));
    let latest_thread = latest.clone();

    std::thread::Builder::new()
        .name("nuc-sampler".into())
        .spawn(move || {
            let mut alerts = AlertEngine::new(alert_prefs);
            let mut current = engine.interval;
            'run: loop {
                // drain control messages
                loop {
                    match cmd_rx.try_recv() {
                        Err(mpsc::TryRecvError::Disconnected) => break 'run,
                        Err(mpsc::TryRecvError::Empty) => break,
                        Ok(Command::Stop) => break 'run,
                        Ok(Command::AdjustInterval(d)) => {
                            current = d.clamp(Duration::from_millis(500), Duration::from_secs(60));
                            engine.set_interval(current);
                        }
                        Ok(Command::SetGpuEnabled(enabled)) => engine.set_gpu_enabled(enabled),
                    }
                }

                let mut snap = engine.sample();
                for event in alerts.evaluate(&snap) {
                    let _ = alert_tx.send(event);
                }
                if let Some(cb) = on_snapshot.as_mut() {
                    cb(&mut snap);
                }
                if let Ok(mut g) = latest_thread.lock() {
                    *g = Some(snap);
                }

                match cmd_rx.recv_timeout(current) {
                    Err(mpsc::RecvTimeoutError::Disconnected) | Ok(Command::Stop) => break 'run,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Ok(Command::AdjustInterval(d)) => {
                        current = d.clamp(Duration::from_millis(500), Duration::from_secs(60));
                        engine.set_interval(current);
                    }
                    Ok(Command::SetGpuEnabled(enabled)) => engine.set_gpu_enabled(enabled),
                }
            }
            drop(engine); // reap GPU children before reporting shutdown complete
            // inform the owner the sampler has exited
            let _ = done_tx.send(());
        })
        .map_err(|e| nuc_core::AppError::Other(e.to_string()))?;

    Ok((
        SampleHandle { tx: cmd_tx, latest },
        SamplerControls {
            alerts: alert_rx,
            done: done_rx,
        },
    ))
}

/// Owns the provider monitors and produces a full `Snapshot`.
pub struct Engine {
    cpu: CpuMonitor,
    thermal: ThermalMonitor,
    fan: FanMonitor,
    rapl: RaplMonitor,
    memory: MemoryMonitor,
    storage: StorageMonitor,
    gpu: GpuMonitor,
    gpu_enabled: bool,
    gpu_attempted: bool,
    interval: Duration,
    prev_ts: Option<i64>,
    peak_power: Option<f64>,
}

impl Engine {
    pub fn build(paths: &SourcePaths, config: &SamplerConfig) -> nuc_core::Result<Engine> {
        let topology = topologyparse::discover(paths)?;
        let interval = config
            .interval
            .clamp(Duration::from_millis(500), Duration::from_secs(60));
        Ok(Engine {
            cpu: CpuMonitor::new(paths.clone(), topology),
            thermal: ThermalMonitor::new(paths.clone()),
            fan: FanMonitor::new(paths.clone()),
            rapl: RaplMonitor::new(paths.clone()),
            memory: MemoryMonitor::new(paths.clone()),
            storage: StorageMonitor::new(paths.clone()),
            gpu: GpuMonitor::new(paths.clone(), nuc_core::millis_u64(interval)),
            gpu_enabled: config.gpu_enabled,
            gpu_attempted: false,
            interval,
            prev_ts: None,
            peak_power: None,
        })
    }

    pub fn gpu_monitor(&self) -> &GpuMonitor {
        &self.gpu
    }

    pub fn storage(&self) -> &StorageMonitor {
        &self.storage
    }

    pub fn start_gpu_utilization(&mut self) -> Result<(), nuc_core::AppError> {
        self.gpu_attempted = true;
        if self.gpu_enabled {
            self.gpu.start_utilization()
        } else {
            Err(nuc_core::AppError::Unsupported(
                "GPU monitoring disabled in settings".into(),
            ))
        }
    }

    fn set_gpu_enabled(&mut self, enabled: bool) {
        if self.gpu_enabled != enabled {
            self.gpu_enabled = enabled;
            self.gpu_attempted = false;
            if !enabled {
                self.gpu.stop_utilization();
            }
        }
    }

    fn set_interval(&mut self, interval: Duration) {
        if self.interval != interval {
            self.interval = interval;
            self.gpu.set_interval_ms(nuc_core::millis_u64(interval));
            self.gpu_attempted = false;
        }
    }

    /// Suspend/stall detection: first sample or a gap longer than 4× the
    /// sampling interval invalidates delta-based values.
    fn deltas_valid(&mut self) -> (bool, i64) {
        // Wall time includes suspend; CLOCK_MONOTONIC does not on Linux.
        let now = nuc_core::utc_now_ms();
        let nominal_gap = (4 * nuc_core::millis_i64(self.interval)).max(8_000);
        let (valid, dt) = match self.prev_ts {
            None => (false, 0),
            Some(prev) => {
                let dt = now.saturating_sub(prev);
                (dt > 0 && dt < nominal_gap, dt.max(0))
            }
        };
        self.prev_ts = Some(now);
        (valid, dt)
    }

    pub fn sample(&mut self) -> Snapshot {
        self.gpu.discover(); // idempotent; bounded device listing
        if self.gpu_enabled && !self.gpu_attempted {
            let _ = self.start_gpu_utilization();
        }
        let (valid, dt_ms) = self.deltas_valid();
        let mut snap = Snapshot {
            ts_ms: nuc_core::utc_now_ms(),
            delta_ms: dt_ms,
            deltas_valid: valid,
            ..Default::default()
        };
        snap.cpu = self.cpu.sample(valid);
        if let Some(p) = snap.cpu.package_power_w
            && self.peak_power.is_none_or(|old| p > old)
        {
            self.peak_power = Some(p);
        }
        snap.peak_power_w = self.peak_power;
        snap.temperatures = self.thermal.sample_temperatures();
        snap.fans = self.fan.sample_fans();
        snap.rapl = self.rapl.sample(valid);
        snap.memory = self.memory.sample();
        snap.storage = self.storage.discover();
        snap.gpu = if self.gpu_enabled {
            self.gpu.sample()
        } else {
            None
        };
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (std::path::PathBuf, SourcePaths) {
        let root = std::env::temp_dir().join(format!("nuc-engine-{name}-{}", std::process::id()));
        let dir = root.join("sys/devices/system/cpu/cpu0/topology");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("core_id"), "0").unwrap();
        let paths = SourcePaths::fixture(&root);
        (root, paths)
    }

    #[test]
    fn long_interval_and_suspend_use_the_current_interval() {
        let (root, paths) = fixture("gap");
        let mut engine = Engine::build(
            &paths,
            &SamplerConfig {
                interval: Duration::from_secs(60),
                gpu_enabled: false,
                record: false,
            },
        )
        .unwrap();
        engine.prev_ts = Some(nuc_core::utc_now_ms() - 60_000);
        assert!(engine.deltas_valid().0);
        engine.set_interval(Duration::from_secs(2));
        engine.prev_ts = Some(nuc_core::utc_now_ms() - 60_000);
        assert!(!engine.deltas_valid().0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dropping_last_handle_stops_sampler_and_publishes_enrichment() {
        let (root, paths) = fixture("stop");
        let alerts = nuc_core::config::Config::default().alerts;
        let prefs = AlertPrefs {
            cpu_temp_warm_c: None,
            nvme_temp_warm_c: None,
            fan_fault: false,
            throttle_warn: false,
            hysteresis_c: alerts.hysteresis_c,
            above_samples: 1,
            cooldown_s: 0,
        };
        let (published, received) = mpsc::channel();
        let (handle, controls) = start_sampler(
            paths,
            SamplerConfig {
                interval: Duration::from_secs(60),
                gpu_enabled: false,
                record: false,
            },
            prefs,
            Some(Box::new(move |snap| {
                snap.peak_power_w = Some(12.0);
                published.send(()).unwrap();
            })),
        )
        .unwrap();
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        let latest = handle.latest.clone();
        drop(handle);
        controls.done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            latest.lock().unwrap().as_ref().unwrap().peak_power_w,
            Some(12.0)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
