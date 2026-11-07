//! Filesystem access abstraction. All kernel reads go through `SourcePaths`
//! so tests can point providers at fixture trees instead of the real system
//! (fixtures in `tests/fixtures/`). Nothing here runs shell commands.

use nuc_core::AppError;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct SourcePaths {
    /// Prefix root for fixture injection ("virtual root"); empty for real system.
    pub sys: PathBuf,
    pub proc: PathBuf,
    pub run: PathBuf,
    pub dev: PathBuf,
    pub class_drm: PathBuf,
    pub class_hwmon: PathBuf,
    pub class_powercap: PathBuf,
    pub class_thermal: PathBuf,
    pub class_block: PathBuf,
    pub sys_class_dmi: PathBuf,
    pub cpu: PathBuf,
}

impl SourcePaths {
    /// Real system paths.
    pub fn system() -> Self {
        let sys = PathBuf::from("/sys");
        let proc_ = PathBuf::from("/proc");
        SourcePaths {
            class_drm: sys.join("class/drm"),
            class_hwmon: sys.join("class/hwmon"),
            class_powercap: sys.join("class/powercap"),
            class_thermal: sys.join("class/thermal"),
            class_block: sys.join("class/block"),
            sys_class_dmi: sys.join("class/dmi/id"),
            cpu: sys.join("devices/system/cpu"),
            sys,
            proc: proc_,
            run: PathBuf::from("/run"),
            dev: PathBuf::from("/dev"),
        }
    }

    /// Fixture injection: `<root>/sys`, `<root>/proc`, `<root>/dev`.
    pub fn fixture(root: &std::path::Path) -> Self {
        SourcePaths {
            sys: root.join("sys"),
            proc: root.join("proc"),
            run: root.join("run"),
            dev: root.join("dev"),
            class_drm: root.join("sys/class/drm"),
            class_hwmon: root.join("sys/class/hwmon"),
            class_powercap: root.join("sys/class/powercap"),
            class_thermal: root.join("sys/class/thermal"),
            class_block: root.join("sys/class/block"),
            sys_class_dmi: root.join("sys/class/dmi/id"),
            cpu: root.join("sys/devices/system/cpu"),
        }
    }
}

pub fn read_string(p: &std::path::Path) -> nuc_core::Result<String> {
    let s = std::fs::read_to_string(p)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AppError::NotFound(p.display().to_string()),
            _ => AppError::from(e),
        })?
        .trim()
        .to_string();
    Ok(s)
}

pub fn read_string_opt(p: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn parse_u64_opt(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

pub fn parse_f64_opt(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

pub fn read_f64_opt(p: &std::path::Path) -> Option<f64> {
    read_string_opt(p).and_then(|s| parse_f64_opt(&s))
}

pub fn read_u64_opt(p: &std::path::Path) -> Option<u64> {
    read_string_opt(p).and_then(|s| parse_u64_opt(&s))
}

pub fn read_i64_opt(p: &std::path::Path) -> Option<i64> {
    read_string_opt(p).and_then(|s| s.parse().ok())
}

pub fn hwmon_device_id(dir: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(dir.join("device"))
        .or_else(|_| std::fs::canonicalize(dir))
        .unwrap_or_else(|_| dir.to_path_buf())
}
