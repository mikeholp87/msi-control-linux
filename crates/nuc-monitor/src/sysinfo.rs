//! Static system/platform detection: DMI, kernel, distro, desktop session,
//! installed power-management services. Used by the capability report and
//! diagnostics. All of it is user-readable or reported as restricted.

use crate::fs::{self, SourcePaths};
use nuc_core::{Capability, SystemIdentity};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServiceState {
    pub name: String,
    pub unit_file_present: bool,
    pub bus_name_active: bool, // only where a well-known D-Bus name is checked transitively
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SystemInfo {
    pub identity: SystemIdentity,
    pub kernel: String,
    pub distro: String,
    /// Wayland / X11 (session type detected from the environment).
    pub session_type: String,
    /// Desktop environment(s) seen in the environment.
    pub desktop_env: String,
    pub services: Vec<ServiceState>,
    pub dbus_accessible: bool,
}

/// Read DMI identity fields; empty when files are unreadable (they are
/// mode-restricted on some setups).
pub fn read_identity(paths: &SourcePaths) -> SystemIdentity {
    let f = |name: &str| -> String {
        fs::read_string_opt(&paths.sys_class_dmi.join(name)).unwrap_or_default()
    };
    SystemIdentity {
        vendor: f("sys_vendor"),
        product: f("product_name"),
        board: f("board_name"),
        bios_vendor: f("bios_vendor"),
        bios_version: f("bios_version"),
        bios_date: f("bios_date"),
    }
}

pub fn discover() -> SystemInfo {
    let paths = SourcePaths::system();
    let kernel = fs::read_string_opt(&std::path::PathBuf::from("/proc/sys/kernel/osrelease"))
        .unwrap_or_default();
    let distro = os_release_pretty();
    SystemInfo {
        identity: read_identity(&paths),
        kernel,
        distro,
        session_type: std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        desktop_env: [
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
            std::env::var("KDE_FULL_SESSION")
                .map(|v| {
                    if v == "true" {
                        "KDE".into()
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" "),
        services: detect_services(),
        dbus_accessible: std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
            .map(|v| !v.is_empty())
            .unwrap_or(false),
    }
}

fn os_release_pretty() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|l| {
                l.strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim_matches('"').to_string())
            })
        })
        .unwrap_or_default()
}

fn detect_services() -> Vec<ServiceState> {
    // unit-file presence is a static check; running state is validated via
    // D-Bus in nuc-power (keeps this module free of async D-Bus deps).
    let mut out = Vec::new();
    for (name, unit_file) in [
        ("power-profiles-daemon", "power-profiles-daemon.service"),
        ("thermald", "thermald.service"),
        ("tuned", "tuned.service"),
        ("tlp", "tlp.service"),
    ] {
        let found = [
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
            "/etc/systemd/system",
        ]
        .iter()
        .any(|dir| std::path::Path::new(dir).join(unit_file).exists());
        out.push(ServiceState {
            name: name.to_string(),
            unit_file_present: found,
            bus_name_active: false,
        });
    }
    out
}

/// DMI property capability (module details often need root).
pub fn dmi_memory_capability() -> Capability {
    match std::path::Path::new("/sys/firmware/dmi/tables/DMI").exists() {
        true => {
            // existence does not mean readability; the tables are mode 0400
            let readable = std::fs::File::open("/sys/firmware/dmi/tables/DMI").is_ok();
            if readable {
                Capability::Available
            } else {
                Capability::RequiresAuthorization
            }
        }
        false => Capability::Unknown,
    }
}
