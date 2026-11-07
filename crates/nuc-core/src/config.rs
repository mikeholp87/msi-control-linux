//! Application configuration, persisted as TOML under `$XDG_CONFIG_HOME`.
//! Corrupted files are reported, ignored and overwritten on next save.

use crate::units::TempUnit;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub monitoring: Monitoring,
    pub alerts: Alerts,
    pub history: History,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// "system" | "dark" | "light"
    pub theme: String,
    pub temp_unit: TempUnit,
    pub chart_refresh_ms: u64,
    /// Initial sidebar page.
    pub start_page: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Monitoring {
    /// Sampling heartbeat for the metering thread (ms).
    pub interval_ms: u64,
    pub gpu_enabled: bool,
    /// Background/extended recording must be explicitly opted in.
    pub record: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Alerts {
    /// CPU (coretemp) temperature threshold in °C. None disables.
    pub cpu_temp_warm_c: Option<f64>,
    /// NVMe temperature warning threshold °C.
    pub nvme_temp_warm_c: Option<f64>,
    pub fan_fault: bool,
    pub throttle_warn: bool,
    /// Hysteresis: how far below the threshold the value must drop to clear.
    pub hysteresis_c: f64,
    /// Debounce: samples above threshold before raising.
    pub above_samples: u32,
    /// Notification cooldown (s).
    pub cooldown_s: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    /// Retention for raw 1-per-sample rows (h).
    pub raw_retention_h: u32,
    /// Retention for 1-minute aggregates (d).
    pub minute_agg_retention_days: u32,
    /// Maximum database size (MiB) before oldest data is dropped.
    pub max_db_mib: u32,
}

impl Default for General {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            temp_unit: TempUnit::Celsius,
            chart_refresh_ms: 1000,
            start_page: "dashboard".into(),
        }
    }
}

impl Default for Monitoring {
    fn default() -> Self {
        Self {
            interval_ms: 2000,
            gpu_enabled: true,
            record: false,
        }
    }
}

impl Default for Alerts {
    fn default() -> Self {
        Self {
            cpu_temp_warm_c: Some(85.0),
            nvme_temp_warm_c: Some(70.0),
            fan_fault: true,
            throttle_warn: true,
            hysteresis_c: 3.0,
            above_samples: 3,
            cooldown_s: 300,
        }
    }
}

impl Default for History {
    fn default() -> Self {
        Self {
            raw_retention_h: 24,
            minute_agg_retention_days: 30,
            max_db_mib: 128,
        }
    }
}

/// XDG-based data and config locations.
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".config")
        });
    base.join("nuc-control").join("config.toml")
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".local/share")
        });
    base.join("nuc-control")
}

impl Config {
    /// Load config; missing => Default; corrupted => Default + warning returned.
    pub fn load() -> (Config, Option<String>) {
        let path = config_path();
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> (Config, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Config>(&text)
                .map_err(|e| e.to_string())
                .and_then(|c| c.validate().map(|_| c).map_err(|e| e.to_string()))
            {
                Ok(c) => (c, None),
                Err(e) => (
                    Config::default(),
                    Some(format!("Ignoring corrupted config {path:?}: {e}")),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
            Err(e) => (
                Config::default(),
                Some(format!("Could not read config {path:?}: {e}")),
            ),
        }
    }

    fn validate(&self) -> std::io::Result<()> {
        let valid = (1000..=60_000).contains(&self.monitoring.interval_ms)
            && (1000..=60_000).contains(&self.general.chart_refresh_ms)
            && self.alerts.cpu_temp_warm_c.is_none_or(f64::is_finite)
            && self.alerts.nvme_temp_warm_c.is_none_or(f64::is_finite)
            && self.alerts.hysteresis_c.is_finite()
            && self.alerts.hysteresis_c >= 0.0
            && self.alerts.above_samples > 0;
        if valid {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "intervals must be 1000–60000 ms; alert thresholds/hysteresis must be finite, hysteresis nonnegative and debounce positive",
            ))
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&config_path())
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text =
            toml::to_string_pretty(self).map_err(|e| std::io::Error::other(e.to_string()))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos();
        let tmp = path.with_extension(format!("toml.{}.{nonce}.tmp", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let result = (|| {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let cfg = Config::default();
        let toml_text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&toml_text).unwrap();
        assert_eq!(back.monitoring.interval_ms, 2000);
        assert_eq!(back.alerts.cpu_temp_warm_c, Some(85.0));
    }

    #[test]
    fn partial_overrides_survive() {
        let just_interval = r#"
            [monitoring]
            interval_ms = 4000
        "#;
        let cfg: Config = toml::from_str(just_interval).unwrap();
        assert_eq!(cfg.monitoring.interval_ms, 4000);
        assert!(cfg.monitoring.gpu_enabled);
        assert_eq!(cfg.alerts.cpu_temp_warm_c, Some(85.0));
    }

    #[test]
    fn omitted_fields_match_documented_defaults() {
        let expected = toml::to_string(&Config::default()).unwrap();
        for text in ["", "[general]", "[monitoring]", "[alerts]", "[history]"] {
            let cfg: Config = toml::from_str(text).unwrap();
            assert_eq!(toml::to_string(&cfg).unwrap(), expected, "input: {text}");
        }
    }

    #[test]
    fn recording_opt_in_survives_roundtrip() {
        for enabled in [false, true] {
            let text = format!("[monitoring]\nrecord = {enabled}");
            let cfg: Config = toml::from_str(&text).unwrap();
            let saved = toml::to_string(&cfg).unwrap();
            let loaded: Config = toml::from_str(&saved).unwrap();
            assert_eq!(loaded.monitoring.record, enabled);
        }
    }

    #[test]
    fn legacy_reserved_keys_are_ignored() {
        let old = "[general]\ncompact_mode = true\n[monitoring]\nrecord_interval_ms = 9000\nbackground_service = true\n";
        let cfg: Config = toml::from_str(old).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn example_matches_defaults() {
        let example: Config = toml::from_str(include_str!(
            "../../../docs/nuc-control-config.toml.example"
        ))
        .unwrap();
        assert_eq!(
            toml::to_string(&example).unwrap(),
            toml::to_string(&Config::default()).unwrap()
        );
    }

    #[test]
    fn corrupted_config_rejected() {
        let broken = r#"
            [monitoring]
            interval_ms = "not a number"
            unrelated = 12
        "#;
        let parsed = toml::from_str::<Config>(broken);
        assert!(parsed.is_err());
    }

    #[test]
    fn invalid_values_warn_and_use_defaults() {
        let dir =
            std::env::temp_dir().join(format!("nuc-config-validation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        for text in [
            "[monitoring]\ninterval_ms = 0",
            "[monitoring]\ninterval_ms = 18446744073709551615",
            "[alerts]\ncpu_temp_warm_c = nan",
            "[alerts]\nhysteresis_c = -1.0",
            "[alerts]\nabove_samples = 0",
        ] {
            std::fs::write(&path, text).unwrap();
            let (cfg, warning) = Config::load_from(&path);
            assert!(warning.is_some(), "{text}");
            assert_eq!(cfg.monitoring.interval_ms, 2000);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_does_not_follow_old_predictable_temporary_symlink() {
        let dir = std::env::temp_dir().join(format!("nuc-config-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let victim = dir.join("other-file");
        std::fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, path.with_extension("toml.tmp")).unwrap();
        let mut cfg = Config::default();
        cfg.monitoring.record = true;
        cfg.save_to(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(Config::load_from(&path).0.monitoring.record);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
