//! User-configurable alerts with hysteresis and debouncing.
//!
//! An alert raises only after `above_samples` consecutive samples beyond the
//! threshold, clears `hysteresis_c` degrees below it, and re-notifies no
//! more often than `cooldown_s`. Nothing here changes hardware settings.

use nuc_core::{AlertEvent, AlertKind, AlertPrefs, Snapshot, utc_now_ms};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlertLevel {
    Notice,
    Warning,
    Critical,
}

impl AlertLevel {
    fn as_str(&self) -> &'static str {
        match self {
            AlertLevel::Notice => "notice",
            AlertLevel::Warning => "warning",
            AlertLevel::Critical => "critical",
        }
    }
}

pub struct AlertEngine {
    prefs: AlertPrefs,
    /// rule -> consecutive qualifying samples
    samples: BTreeMap<AlertKind, u32>,
    last_sent: BTreeMap<AlertKind, Instant>,
    active: BTreeMap<AlertKind, bool>,
}

impl AlertEngine {
    pub fn new(prefs: AlertPrefs) -> Self {
        AlertEngine {
            prefs,
            samples: BTreeMap::new(),
            last_sent: BTreeMap::new(),
            active: BTreeMap::new(),
        }
    }

    pub fn set_prefs(&mut self, prefs: AlertPrefs) {
        self.prefs = prefs;
    }

    /// Temperature rule shared by CPU/NVME checks.
    fn temperature_rule(
        &mut self,
        kind: AlertKind,
        level: AlertLevel,
        value: Option<f64>,
        threshold: f64,
        message_base: &str,
        events: &mut Vec<AlertEvent>,
    ) {
        match value {
            Some(v) if v > threshold => {
                let count = self.samples.entry(kind).or_insert(0);
                *count = count.saturating_add(1);
                let ready = *count >= self.prefs.above_samples.max(1);
                let not_active = !self.active.get(&kind).copied().unwrap_or(false);
                let cooled = self
                    .last_sent
                    .get(&kind)
                    .map(|t| t.elapsed().as_secs() >= self.prefs.cooldown_s)
                    .unwrap_or(true);
                if ready && not_active && cooled {
                    self.active.insert(kind, true);
                    self.last_sent.insert(kind, Instant::now());
                    events.push(AlertEvent {
                        ts_ms: utc_now_ms(),
                        kind,
                        level: level.as_str().into(),
                        message: format!("{message_base} ({v:.1} °C > {threshold:.0} °C)"),
                        value: Some(v),
                        threshold: Some(threshold),
                    });
                }
            }
            Some(v) => {
                self.samples.insert(kind, 0);
                // below threshold + hysteresis clears the armed state
                if v < threshold - self.prefs.hysteresis_c {
                    self.samples.insert(kind, 0);
                    self.active.remove(&kind);
                }
            }
            None => {
                self.samples.insert(kind, 0);
            }
        }
    }

    pub fn evaluate(&mut self, snap: &Snapshot) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        if let Some(threshold) = self.prefs.cpu_temp_warm_c {
            let cpu_temp = snap
                .temperatures
                .iter()
                .filter(|t| t.source == "coretemp")
                .filter_map(|t| t.value_c)
                .reduce(f64::max);
            self.temperature_rule(
                AlertKind::CpuTemp,
                AlertLevel::Warning,
                cpu_temp,
                threshold,
                "CPU temperature exceeds the configured threshold",
                &mut events,
            );
        }
        if let Some(threshold) = self.prefs.nvme_temp_warm_c {
            let nvme_temp = snap
                .temperatures
                .iter()
                .filter(|t| t.source == "nvme")
                .filter_map(|t| t.value_c)
                .reduce(f64::max);
            self.temperature_rule(
                AlertKind::NvmeTemp,
                AlertLevel::Warning,
                nvme_temp,
                threshold,
                "SSD temperature exceeds the configured threshold",
                &mut events,
            );
        }
        if self.prefs.fan_fault {
            for fan in snap.fans.iter().filter(|f| f.fault == Some(true)) {
                if self
                    .last_sent
                    .get(&AlertKind::FanFault)
                    .map(|t| t.elapsed().as_secs() >= self.prefs.cooldown_s)
                    .unwrap_or(true)
                {
                    self.last_sent.insert(AlertKind::FanFault, Instant::now());
                    events.push(AlertEvent {
                        ts_ms: utc_now_ms(),
                        kind: AlertKind::FanFault,
                        level: AlertLevel::Critical.as_str().into(),
                        message: format!("Fan reports fault: {}", fan.label),
                        value: fan.rpm,
                        threshold: None,
                    });
                }
            }
        }
        if self.prefs.throttle_warn {
            let new_core = snap.cpu.new_core_throttles.unwrap_or(0);
            let new_pkg = snap.cpu.new_package_throttles.unwrap_or(0);
            if (new_core > 0 || new_pkg > 0)
                && self
                    .last_sent
                    .get(&AlertKind::Throttle)
                    .map(|t| t.elapsed().as_secs() >= self.prefs.cooldown_s)
                    .unwrap_or(true)
            {
                self.last_sent.insert(AlertKind::Throttle, Instant::now());
                events.push(AlertEvent {
                    ts_ms: utc_now_ms(),
                    kind: AlertKind::Throttle,
                    level: AlertLevel::Notice.as_str().into(),
                    message: if new_pkg > 0 {
                        "Package thermal throttling events increased during this interval".into()
                    } else {
                        "Core thermal throttling events increased during this interval".into()
                    },
                    value: Some((new_core + new_pkg) as f64),
                    threshold: None,
                });
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuc_core::{CpuSnapshot, Temperature, ThrottleInfo};

    fn snap_with(temp_c: Option<f64>, throttle_delta: Option<u64>) -> Snapshot {
        Snapshot {
            ts_ms: utc_now_ms(),
            cpu: CpuSnapshot {
                throttle: ThrottleInfo {
                    supported: true,
                    ..Default::default()
                },
                new_core_throttles: throttle_delta,
                ..Default::default()
            },
            temperatures: vec![Temperature {
                id: "coretemp/temp1".into(),
                label: "Package id 0".into(),
                value_c: temp_c,
                source: "coretemp".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn prefs() -> AlertPrefs {
        AlertPrefs {
            cpu_temp_warm_c: Some(85.0),
            nvme_temp_warm_c: Some(70.0),
            fan_fault: true,
            throttle_warn: true,
            hysteresis_c: 3.0,
            above_samples: 2,
            cooldown_s: 0,
        }
    }

    #[test]
    fn debounce_requires_consecutive_breaches() {
        let mut e = AlertEngine::new(prefs());
        assert!(e.evaluate(&snap_with(Some(92.0), None)).is_empty());
        let events = e.evaluate(&snap_with(Some(92.0), None));
        assert_eq!(events.len(), 1, "second consecutive over-threshold raises");
    }

    #[test]
    fn hysteresis_prevents_flapping() {
        let mut e = AlertEngine::new(prefs());
        let _ = e.evaluate(&snap_with(Some(92.0), None));
        let _ = e.evaluate(&snap_with(Some(92.0), None));
        // deep bounce below threshold - hysteresis should clear
        let _ = e.evaluate(&snap_with(Some(80.0), None));
        let again = e.evaluate(&snap_with(Some(92.0), None));
        assert!(again.is_empty(), "re-arm needs above_samples again");
    }

    #[test]
    fn throttle_delta_notices() {
        let mut e = AlertEngine::new(prefs());
        let events = e.evaluate(&snap_with(None, Some(1)));
        assert!(events.iter().any(|ev| ev.message.contains("throttling")));
    }

    #[test]
    fn below_threshold_breaks_consecutive_breaches_inside_hysteresis_band() {
        let mut engine = AlertEngine::new(prefs());
        assert!(engine.evaluate(&snap_with(Some(90.0), None)).is_empty());
        assert!(engine.evaluate(&snap_with(Some(84.0), None)).is_empty());
        assert!(engine.evaluate(&snap_with(Some(90.0), None)).is_empty());
        assert_eq!(engine.evaluate(&snap_with(Some(90.0), None)).len(), 1);
    }

    #[test]
    fn hottest_cpu_sensor_triggers_even_if_first_sensor_is_cool() {
        let mut engine = AlertEngine::new(AlertPrefs {
            above_samples: 1,
            ..prefs()
        });
        let mut snap = snap_with(Some(40.0), None);
        let mut hot = snap.temperatures[0].clone();
        hot.value_c = Some(95.0);
        snap.temperatures.push(hot);
        assert_eq!(engine.evaluate(&snap).len(), 1);
    }
}
