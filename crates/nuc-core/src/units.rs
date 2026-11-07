//! Unit conversion and formatting helpers.
//!
//! All kernel values are converted explicitly at the boundary — hwmon
//! temperatures are milli-degrees, RAPL energy is microjoules, CPUFreq
//! frequencies are kHz. Nothing downstream ever re-scales raw values.

use serde::{Deserialize, Serialize};

/// Hwmon milli-degrees Celsius -> degrees Celsius.
pub fn millidegree_to_celsius(md: i64) -> f64 {
    md as f64 / 1000.0
}

/// RAPL microjoules over an interval (seconds) -> average watts.
/// Returns `None` for non-positive or huge elapsed time (sampling gap).
pub fn energy_uj_to_watts(uj: u64, elapsed_s: f64) -> Option<f64> {
    if !elapsed_s.is_finite() || elapsed_s <= 0.0 || elapsed_s > 3_600.0 {
        return None;
    }
    Some((uj as f64 / 1e6) / elapsed_s)
}

/// kHz (CPUFreq) -> MHz
pub fn khz_to_mhz(khz: u64) -> u64 {
    khz / 1000
}

/// Kelvin (NVMe / UDisks) -> Celsius
pub fn kelvin_to_celsius(k: u64) -> f64 {
    k as f64 - 273.15
}

/// Bytes -> human-readable string (binary units).
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut scale = 1.0f64;
    let mut idx = 0usize;
    while idx + 1 < UNITS.len() && bytes as f64 / scale >= 1024.0 {
        scale *= 1024.0;
        idx += 1;
    }
    format!("{:.1} {}", bytes as f64 / scale, UNITS[idx])
}

/// kHz -> "3.90 GHz" style string.
pub fn fmt_khz(khz: u64) -> String {
    let mhz = khz as f64 / 1000.0;
    if mhz >= 1000.0 {
        format!("{:.2} GHz", mhz / 1000.0)
    } else {
        format!("{mhz:.0} MHz")
    }
}

pub fn fmt_watts(w: f64) -> String {
    format!("{w:.2} W")
}

/// Display unit for temperatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TempUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

impl TempUnit {
    pub fn format(&self, celsius: f64) -> String {
        match self {
            TempUnit::Celsius => format!("{celsius:.1} °C"),
            TempUnit::Fahrenheit => format!("{:.1} °F", celsius * 9.0 / 5.0 + 32.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn millidegree() {
        close(millidegree_to_celsius(57000), 57.0);
    }

    #[test]
    fn energy_to_watts() {
        // 2,000,000 µJ in 1 s -> 2 W
        close(energy_uj_to_watts(2_000_000, 1.0).unwrap(), 2.0);
        assert!(energy_uj_to_watts(100, 0.0).is_none()); // invalid interval
        assert!(energy_uj_to_watts(100, f64::NAN).is_none());
        // elapsed > 3600 s (suspend gap) is treated as a sampling gap
        assert!(energy_uj_to_watts(3_600_000_001, 3_600.1).is_none());
    }

    #[test]
    fn bytes_format() {
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(2_000_116_990_976), "1.8 TiB");
    }

    #[test]
    fn khz_format() {
        assert_eq!(fmt_khz(3_905_051), "3.91 GHz");
        assert_eq!(fmt_khz(400_000), "400 MHz");
    }

    #[test]
    fn celsius_fahrenheit() {
        assert_eq!(TempUnit::Fahrenheit.format(100.0), "212.0 °F");
        assert_eq!(TempUnit::Celsius.format(25.5), "25.5 °C");
    }

    #[test]
    fn acpi_fan_values_are_not_labelled_rpm() {
        let fan = crate::FanReading {
            rpm: Some(61.0),
            source: "acpi_fan".into(),
            ..Default::default()
        };
        assert_eq!(fan.formatted_speed(), "61 (ACPI-reported value)");
        assert!(!fan.formatted_speed().contains("RPM"));
    }
}
