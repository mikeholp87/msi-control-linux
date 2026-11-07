//! Entry: builds the native GTK4/libadwaita window and wires the sampler to
//! the UI. The GUI itself is always unprivileged: it only reads /sys//proc
//! and talks to D-Bus daemons (power-profiles-daemon, UDisks2).

mod chart;
mod history_worker;
mod pages;

use adw::prelude::*;
use libadwaita as adw;
use std::sync::{Arc, Mutex};

use nuc_core::MutexExt;
use nuc_core::config::Config;
use nuc_history::HistoryStore;
use nuc_monitor::engine::{SampleHandle, SamplerConfig, SamplerControls, start_sampler};

/// State shared between the sampler thread and the UI tick.
pub struct GuiState {
    pub handle: SampleHandle,
    pub controls: SamplerControls,
    pub history: history_worker::HistoryWorker,
    pub cfg: Config,
    /// Series the chart draws (main-thread only, guarded by the state lock).
    pub series: SeriesData,
    pub tick_count: u64,
    pub profile_stop: std::sync::mpsc::Sender<()>,
}

impl GuiState {
    pub fn shared(self) -> Arc<Mutex<GuiState>> {
        Arc::new(Mutex::new(self))
    }
}

/// A few parallel time series for the chart (t = seconds since window start).
#[derive(Default)]
pub struct SeriesData {
    pub start_ms: Option<i64>,
    pub cpu_temp: Vec<(f64, Option<f64>)>,
    pub pkg_power: Vec<(f64, Option<f64>)>,
    pub cpu_util: Vec<(f64, Option<f64>)>,
    pub last_ts_ms: Option<i64>,
    /// Cap so long sessions stay bounded (~2h at the default cadence).
    pub max_kept: usize,
}

impl SeriesData {
    pub fn push(&mut self, snap: &nuc_core::Snapshot) -> bool {
        if self.last_ts_ms == Some(snap.ts_ms) {
            return false;
        }
        self.last_ts_ms = Some(snap.ts_ms);
        let start = *self.start_ms.get_or_insert(snap.ts_ms);
        let t = (snap.ts_ms - start) as f64 / 1000.0;
        let trim = |v: &mut Vec<(f64, Option<f64>)>| {
            let keep = self.max_kept.max(2);
            if v.len() > keep {
                v.drain(..v.len() - keep);
            }
        };
        let pkg = snap
            .temperatures
            .iter()
            .filter(|x| x.source.contains("coretemp") || x.id.contains("coretemp"))
            .filter_map(|x| x.value_c)
            .reduce(f64::max);
        if !snap.deltas_valid && !self.cpu_temp.is_empty() {
            self.cpu_temp.push((t, None));
        }
        self.cpu_temp.push((t, pkg));
        self.pkg_power
            .push((t, snap.cpu.interval_power_w.filter(|_| snap.deltas_valid)));
        self.cpu_util
            .push((t, snap.cpu.usage_total.filter(|_| snap.deltas_valid)));
        trim(&mut self.cpu_temp);
        trim(&mut self.pkg_power);
        trim(&mut self.cpu_util);
        true
    }
}

fn main() {
    let application = adw::Application::builder()
        .application_id("org.nuccontrol.gui")
        .build();

    application.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        let (config, mut warning) = Config::load();
        if let Some(w) = &warning {
            eprintln!("config warning: {w}");
        }
        let paths = nuc_monitor::fs::SourcePaths::system();

        let sampler_cfg = SamplerConfig {
            interval: std::time::Duration::from_millis(config.monitoring.interval_ms.max(1000)),
            gpu_enabled: config.monitoring.gpu_enabled,
            record: config.monitoring.record,
        };
        let prefs = nuc_core::AlertPrefs {
            cpu_temp_warm_c: config.alerts.cpu_temp_warm_c,
            nvme_temp_warm_c: config.alerts.nvme_temp_warm_c,
            fan_fault: config.alerts.fan_fault,
            throttle_warn: config.alerts.throttle_warn,
            hysteresis_c: config.alerts.hysteresis_c,
            above_samples: config.alerts.above_samples,
            cooldown_s: config.alerts.cooldown_s,
        };

        let profile = Arc::new(Mutex::new(None));
        let profile_worker = profile.clone();
        let (profile_stop, stop_profile) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let state = nuc_power::ProfileClient::new()
                    .and_then(|client| client.state())
                    .ok();
                *profile_worker.lock_or_recover() = state;
                match stop_profile.recv_timeout(std::time::Duration::from_secs(2)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        let (handle, controls) = start_sampler(
            paths,
            sampler_cfg,
            prefs,
            Some(Box::new(move |snap| {
                snap.profile = profile.lock_or_recover().clone();
            })),
        )
        .unwrap_or_else(|e| {
            eprintln!("sampler failed to start: {e}");
            std::process::exit(1);
        });

        let (history, persistent_history) = match HistoryStore::open_default() {
            Ok(store) => (store, true),
            Err(e) => {
                let text = format!("History is session-only; persistent database unavailable: {e}");
                eprintln!("{text}");
                warning = Some(match warning {
                    Some(w) => format!("{w}\n{text}"),
                    None => text,
                });
                (
                    HistoryStore::open_in_memory().expect("in-memory history store"),
                    false,
                )
            }
        };

        let state = GuiState {
            handle,
            controls,
            history: history_worker::HistoryWorker::start(history, persistent_history),
            cfg: config,
            series: SeriesData {
                max_kept: 2400,
                ..Default::default()
            },
            tick_count: 0,
            profile_stop,
        }
        .shared();

        pages::build(app, state, warning);
    });

    application.run();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn series_ingests_new_samples_once_and_keeps_gaps_and_bounds() {
        let mut series = SeriesData {
            max_kept: 2,
            ..Default::default()
        };
        let mut snap = nuc_core::Snapshot {
            ts_ms: 1000,
            deltas_valid: true,
            ..Default::default()
        };
        snap.cpu.usage_total = Some(20.0);
        assert!(series.push(&snap));
        assert!(!series.push(&snap));
        snap.ts_ms += 1000;
        snap.deltas_valid = false;
        series.push(&snap);
        assert_eq!(series.cpu_util.last().unwrap().1, None);
        snap.ts_ms += 1000;
        series.push(&snap);
        assert_eq!(series.cpu_util.len(), 2);
    }
}
