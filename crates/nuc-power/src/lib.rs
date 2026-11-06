//! Power profile management and authorized D-Bus integration.
//!
//! power-profiles-daemon owns CPU policy changes on this system (polkit-
//! authorized). NUC Control talks to its documented D-Bus interface, verifies
//! every change by re-reading the daemon's answer, and never disables or
//! replaces the daemon.
//!
//! NVMe SMART attributes come from UDisks2 via `NVMe.Controller.SmartGetAttributes`
//! on the drive object (authorized for the active user session through polkit;
//! the raw /dev/nvme* char device is not user-readable). The application stays
//! root-free at all times.

use nuc_core::{AppError, NvmeHealth, PowerProfileState};
use std::collections::HashMap;
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Profiler bus names, newest documented first (both may be active; upstream
/// renamed the interface over the years and daemons often keep both).
const PPD_INTERFACES: [&str; 2] = [
    "org.freedesktop.UPower.PowerProfiles",
    "net.hadess.PowerProfiles",
];
/// The daemon always owns this object path under either bus name.
const PPD_PATH: &str = "/net/hadess/PowerProfiles";

/// Desired power profile names in canonical interface vocabulary.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Profile {
    PowerSaver,
    Balanced,
    Performance,
}

impl Profile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::PowerSaver => "power-saver",
            Profile::Balanced => "balanced",
            Profile::Performance => "performance",
        }
    }

    /// Human aliases accepted by the API (validation stays strict underneath).
    pub fn from_arg(arg: &str) -> Result<Profile, AppError> {
        match arg.to_ascii_lowercase().replace('_', "-").as_str() {
            "power-saver" | "powersaver" | "quiet" => Ok(Profile::PowerSaver),
            "balanced" => Ok(Profile::Balanced),
            "performance" => Ok(Profile::Performance),
            _ => Err(AppError::Unsupported(format!(
                "unknown profile '{arg}' (supported: power-saver, balanced, performance)"
            ))),
        }
    }
}

/// Handles raw `org.freedesktop.DBus.Properties` Get/Set against one object.
struct PropertyBus<'c> {
    conn: &'c Connection,
    dest: String,
    path: String,
}

impl PropertyBus<'_> {
    fn proxy(&self) -> nuc_core::Result<zbus::blocking::Proxy<'_>> {
        zbus::blocking::Proxy::new(
            self.conn,
            self.dest.as_str(),
            self.path.as_str(),
            "org.freedesktop.DBus.Properties",
        )
        .map_err(bus_err)
    }

    /// Get one property as an owned (lent-out) variant.
    fn get(&self, iface: &str, name: &str) -> nuc_core::Result<OwnedValue> {
        self.proxy()?.call("Get", &(iface, name)).map_err(bus_err)
    }

    fn set_str(&self, iface: &str, name: &str, value: &str) -> nuc_core::Result<()> {
        let result: Result<(), _> = self
            .proxy()?
            .call("Set", &(iface, name, Value::from(value)));
        result.map_err(bus_err)
    }
}

fn as_str(v: &Value<'_>) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

fn as_u64(v: &Value<'_>) -> Option<u64> {
    match v {
        Value::U8(x) => Some(*x as u64),
        Value::U16(x) => Some(*x as u64),
        Value::U32(x) => Some(*x as u64),
        Value::U64(x) => Some(*x),
        _ => None,
    }
}

/// Profile operations against the running power-profiles-daemon. A fresh
/// proxy is created per call; profile changes are rare, so the overhead is
/// negligible and this avoids self-referential lifetime juggling.
pub struct ProfileClient {
    conn: Connection,
    /// Bus name the daemon actually answered under (resolved in `new()`).
    iface: &'static str,
}

impl ProfileClient {
    /// Connect to the system bus and verify the daemon answers.
    pub fn new() -> nuc_core::Result<Self> {
        let conn = Connection::system().map_err(|e| AppError::Other(format!("system bus: {e}")))?;
        let mut client = ProfileClient {
            conn,
            iface: PPD_INTERFACES[0],
        };
        let mut last_err = None;
        for iface in PPD_INTERFACES {
            match client.state_on(iface) {
                Ok(_) => {
                    client.iface = iface;
                    return Ok(client);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| AppError::Unsupported("power-profiles-daemon not reachable".into())))
    }

    /// State read against one specific daemon interface.
    fn state_on(&self, iface: &str) -> nuc_core::Result<PowerProfileState> {
        let bus = PropertyBus {
            conn: &self.conn,
            dest: iface.to_string(),
            path: PPD_PATH.into(),
        };
        let profiles: OwnedValue = bus.get(iface, "Profiles")?;
        let active: OwnedValue = bus.get(iface, "ActiveProfile")?;
        // 0.30 exposes PerformanceDegraded: s ("" when fine); absence on
        // older versions is treated as "not degraded".
        let degraded_reason: Option<String> = bus
            .get(iface, "PerformanceDegraded")
            .ok()
            .and_then(|v| as_str(&Value::from(v)))
            .filter(|s| !s.is_empty());
        Ok(PowerProfileState {
            backend: format!("power-profiles-daemon ({iface})"),
            active: as_str(&Value::from(active)),
            profiles: profiles_from_value(&Value::from(profiles)),
            degraded: degraded_reason.is_some(),
            degraded_reason,
        })
    }

    /// Current power-profiles-daemon state (profiles, active, degraded cause).
    pub fn state(&self) -> nuc_core::Result<PowerProfileState> {
        self.state_on(self.iface)
    }

    /// Switch profile: validate -> write -> re-read -> confirm (never assume).
    pub fn set_profile_arg(&self, profile: &str) -> nuc_core::Result<PowerProfileState> {
        let wanted = Profile::from_arg(profile)?;
        let state = self.state()?;
        if !state.profiles.iter().any(|p| p == wanted.as_str()) {
            return Err(AppError::Unsupported(format!(
                "profile '{}' is not offered by the system (available: {:?})",
                wanted.as_str(),
                state.profiles
            )));
        }
        let bus = PropertyBus {
            conn: &self.conn,
            dest: self.iface.to_string(),
            path: PPD_PATH.into(),
        };
        bus.set_str(self.iface, "ActiveProfile", wanted.as_str())?;
        let after = self.state()?;
        match after.active.as_deref() {
            Some(a) if a == wanted.as_str() => Ok(after),
            other => Err(AppError::Other(format!(
                "profile switch did not take effect: requested '{}', active now {other:?} \
                 (the platform may have degraded the profile)",
                wanted.as_str(),
            ))),
        }
    }
}

/// Decode the daemon's `Profiles` property (aa{sv}: array of dicts).
fn profiles_from_value(v: &Value<'_>) -> Vec<String> {
    let mut out = Vec::new();
    if let Value::Array(arr) = v {
        for elem in arr.inner() {
            if let Value::Dict(d) = elem {
                for (k, val) in d.iter() {
                    let key = k.downcast_ref::<String>().ok();
                    let prof = val.downcast_ref::<String>().ok();
                    if key.as_deref() == Some("Profile")
                        && let Some(p) = prof
                    {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

fn bus_err(e: zbus::Error) -> AppError {
    let text = e.to_string();
    if text.contains("was not provided by any .service files")
        || text.contains("Name is not activatable")
        || text.contains("NameUnknown")
    {
        AppError::NotFound("power-profiles-daemon not running".into())
    } else {
        AppError::Other(format!("D-Bus: {text}"))
    }
}

// ------------------------------------------------------------ NVMe SMART

/// NVMe health attributes via UDisks2 for one block device (e.g. "/dev/nvme0n1"
/// or any partition on it). Only attributes the backend actually returned are
/// filled in; zero/unimplemented fields become `None` — nothing is inferred.
pub fn nvme_health(device_node: &str) -> nuc_core::Result<NvmeHealth> {
    let conn = Connection::system().map_err(|e| AppError::Other(format!("system bus: {e}")))?;
    let basename = node_basename(device_node);
    let obj = format!("/org/freedesktop/UDisks2/block_devices/{basename}");

    // The NVMe.Controller interface lives on the drive object that the block
    // device points to (verified by introspection on this machine; block
    // devices only expose NVMe.Namespace).
    let bus = PropertyBus {
        conn: &conn,
        dest: "org.freedesktop.UDisks2".to_string(),
        path: obj.clone(),
    };
    let drive: OwnedObjectPath = bus
        .get("org.freedesktop.UDisks2.Block", "Drive")
        .and_then(|v| v.try_into().map_err(map_any))
        .map_err(|_| {
            AppError::Unsupported(format!(
                "{device_node}: not a block device known to UDisks2"
            ))
        })?;

    let ctrl_proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.UDisks2",
        drive.as_str(),
        "org.freedesktop.UDisks2.NVMe.Controller",
    )
    .map_err(|_| {
        AppError::Unsupported(format!(
            "{device_node} is not an NVMe device (no NVMe.Controller interface)"
        ))
    })?;

    let attrs: HashMap<String, OwnedValue> = ctrl_proxy
        .call("SmartGetAttributes", &(HashMap::<String, Value>::new(),))
        .map_err(|e| {
            let text = e.to_string();
            if text.contains("UnknownMethod") || text.contains("No such interface") {
                AppError::Unsupported(format!(
                    "{device_node} is not an NVMe device (no NVMe.Controller interface)"
                ))
            } else {
                AppError::Other(format!("SmartGetAttributes: {text}"))
            }
        })?;
    let mut health = health_from_attrs(
        attrs
            .into_iter()
            .map(|(k, v)| (k, Value::from(v)))
            .collect(),
    );
    let drive_bus = PropertyBus {
        conn: &conn,
        dest: "org.freedesktop.UDisks2".to_string(),
        path: drive.as_str().to_string(),
    };
    let properties: Result<HashMap<String, OwnedValue>, _> = drive_bus
        .proxy()?
        .call("GetAll", &("org.freedesktop.UDisks2.NVMe.Controller",));
    if let Ok(properties) = properties {
        apply_smart_properties(
            &mut health,
            properties
                .into_iter()
                .map(|(k, v)| (k, Value::from(v)))
                .collect(),
        )?;
    }
    Ok(health)
}

fn apply_smart_properties(
    health: &mut NvmeHealth,
    properties: HashMap<String, Value>,
) -> nuc_core::Result<()> {
    if attr_u64(&properties, "SmartUpdated") == Some(0) {
        return Err(AppError::Unsupported(
            "NVMe SMART data has never been updated".into(),
        ));
    }
    if health.power_on_hours.is_none() {
        health.power_on_hours = attr_u64(&properties, "SmartPowerOnHours");
    }
    if let Some(Value::Array(warnings)) = properties.get("SmartCriticalWarning") {
        health.critical_warnings = warnings.inner().iter().map(as_str).collect();
    }
    Ok(())
}

/// Identity mapping for `any` error (used to coerce zvariant TryFrom errors).
fn map_any<E: std::fmt::Display>(e: E) -> AppError {
    let text = e.to_string();
    if text.contains("IncorrectType") {
        AppError::Unsupported("type mismatch reading UDisks2 data".into())
    } else if text.contains("UnknownObject") || text.contains("No such interface") {
        AppError::Unsupported("UDisks2 object not found".into())
    } else {
        AppError::Other(format!("UDisks2: {text}"))
    }
}

/// UDisks2 object paths encode non-alphanumeric characters as `_XX`.
fn node_basename(node: &str) -> String {
    let base = node.rsplit('/').next().unwrap_or(node);
    let mut out = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push_str(&format!("_{:02x}", ch as u32));
        }
    }
    out
}

/// SmartGetAttributes types confirmed on this machine:
///   y = u8   (percent_used, avail_spare, spare_thresh)
///   t = u64  (counters)
///   q = u16  (wctemp, cctemp — Kelvin)
///   aq       (temp_sensors; 0 entries mean "sensor not implemented")
fn attr_u64(attrs: &HashMap<String, Value>, key: &str) -> Option<u64> {
    attrs.get(key).and_then(as_u64)
}

fn attr_u8(attrs: &HashMap<String, Value>, key: &str) -> Option<u8> {
    match attr_u64(attrs, key) {
        Some(v) if v <= u8::MAX as u64 => Some(v as u8),
        _ => None,
    }
}

/// Kelvin (u16) → °C. Zero means "not implemented" per NVMe spec.
fn kelvin(k: u64) -> f64 {
    k as f64 - 273.15
}

fn health_from_attrs(attrs: HashMap<String, Value>) -> NvmeHealth {
    // First non-zero temp sensor (u16 Kelvin). All zeros → no temperature.
    let sensor_k = match attrs.get("temp_sensors") {
        Some(Value::Array(arr)) => arr.inner().iter().find_map(|v| match v {
            Value::U16(k) if *k > 0 => Some(*k),
            _ => None,
        }),
        _ => None,
    };
    let mut health = NvmeHealth {
        percent_used: attr_u8(&attrs, "percent_used"),
        avail_spare_pct: attr_u8(&attrs, "avail_spare"),
        spare_threshold_pct: attr_u8(&attrs, "spare_thresh"),
        units_read: attr_u64(&attrs, "total_data_read"),
        units_written: attr_u64(&attrs, "total_data_written"),
        power_on_hours: attr_u64(&attrs, "power_on_hours"),
        unsafe_shutdowns: attr_u64(&attrs, "unsafe_shutdowns"),
        media_errors: attr_u64(&attrs, "media_errors"),
        error_log_entries: attr_u64(&attrs, "num_err_log_entries"),
        critical_warning: attr_u64(&attrs, "critical_warning"),
        critical_warnings: None,
        warn_temp_c: attr_u64(&attrs, "wctemp").filter(|k| *k > 0).map(kelvin),
        crit_temp_c: attr_u64(&attrs, "cctemp").filter(|k| *k > 0).map(kelvin),
        source: String::new(),
    };
    health.source = match sensor_k {
        Some(k) => format!(
            "UDisks2 SmartGetAttributes (sensor {:.1} °C)",
            kelvin(k as u64)
        ),
        None => "UDisks2 SmartGetAttributes".to_string(),
    };
    health
}

// ------------------------------------------------------------ CONFLICTS

/// Which installed services coexist that could influence power/thermal
/// behavior. Read-only: we only query unit states, never touch anything.
/// Findings are (pretty name, status) pairs for the diagnostics report.
pub fn conflict_scan() -> Vec<(String, String)> {
    let entries = [
        (
            "power-profiles-daemon.service",
            "power-profiles-daemon (primary profile daemon)",
        ),
        ("upower.service", "UPower (desktop power events)"),
        ("thermald.service", "thermald (Linux thermal daemon)"),
        ("tuned.service", "tuned (profile manager)"),
        ("tlp.service", "TLP (laptop power management)"),
        ("cpupower.service", "cpupower (manual governor service)"),
        ("system76-power.service", "system76-power"),
        ("auto-cpufreq.service", "auto-cpufreq"),
    ];
    let conn = match Connection::system().map_err(|e| AppError::Other(format!("system bus: {e}"))) {
        Ok(c) => c,
        Err(e) => return vec![("systemd".into(), format!("system bus unavailable: {e}"))],
    };
    let systemd = match zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    ) {
        Ok(p) => p,
        Err(e) => return vec![("systemd".into(), format!("systemd unreachable: {e}"))],
    };
    entries
        .iter()
        .map(|(unit, pretty)| (pretty.to_string(), unit_state(&systemd, unit)))
        .collect()
}

fn unit_state(systemd: &zbus::blocking::Proxy<'_>, unit: &str) -> String {
    let path: OwnedObjectPath = match systemd.call("GetUnit", &(unit,)) {
        Ok(p) => p,
        Err(e) => {
            let near = e.to_string();
            if near.contains("UnknownUnit") || near.contains("NoSuchUnit") {
                return "not installed".into();
            }
            return format!("unqueried ({near})");
        }
    };
    let bus = PropertyBus {
        conn: systemd.connection(),
        dest: "org.freedesktop.systemd1".to_string(),
        path: path.as_str().to_string(),
    };
    let active = bus
        .get("org.freedesktop.systemd1.Unit", "ActiveState")
        .ok()
        .and_then(|v| as_str(&Value::from(v)))
        .unwrap_or_else(|| "unknown".into());
    let sub = bus
        .get("org.freedesktop.systemd1.Unit", "SubState")
        .ok()
        .and_then(|v| as_str(&Value::from(v)))
        .unwrap_or_else(|| "unknown".into());
    if active == "active" {
        format!("active ({sub}) — may overlap NUC Control behavior")
    } else {
        format!("{active} ({sub})")
    }
}

// ------------------------------------------------------------ TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Signature};

    fn attrs_from<'a>(pairs: Vec<(&'a str, Value<'a>)>) -> HashMap<String, Value<'a>> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    /// Fixture mirrors the types observed live on the WD Black SN850X
    /// (spare 100 %, no wear, all-zero temp sensors = not implemented).
    #[test]
    fn smart_attrs_live_shape() {
        let mut sensors = Array::new(&Signature::U16);
        for k in [0u16; 8] {
            sensors.append(Value::U16(k)).unwrap();
        }
        let attrs = attrs_from(vec![
            ("avail_spare", Value::U8(100)),
            ("spare_thresh", Value::U8(10)),
            ("percent_used", Value::U8(0)),
            ("total_data_read", Value::U64(4974736896000)),
            ("total_data_written", Value::U64(6096767488000)),
            ("power_cycles", Value::U64(127)),
            ("unsafe_shutdowns", Value::U64(4)),
            ("media_errors", Value::U64(0)),
            ("num_err_log_entries", Value::U64(0)),
            ("temp_sensors", Value::Array(sensors)),
            ("wctemp", Value::U16(363)),
            ("cctemp", Value::U16(367)),
        ]);
        let h = health_from_attrs(attrs);
        assert_eq!(h.percent_used, Some(0));
        assert_eq!(h.avail_spare_pct, Some(100));
        assert_eq!(h.spare_threshold_pct, Some(10));
        assert_eq!(h.units_written, Some(6096767488000));
        assert_eq!(h.media_errors, Some(0));
        assert_eq!(h.unsafe_shutdowns, Some(4));
        assert_close(h.warn_temp_c, Some(89.85));
        assert_close(h.crit_temp_c, Some(93.85));
        assert!(h.source.contains("UDisks2"));
        // zero sensors → no temperature invented
        assert!(!h.source.contains('°'));
    }

    #[test]
    fn smart_attrs_worn_device() {
        let attrs = attrs_from(vec![
            ("percent_used", Value::U8(46)),
            ("avail_spare", Value::U8(90)),
            ("spare_thresh", Value::U8(10)),
            ("media_errors", Value::U64(2)),
            ("critical_warning", Value::U64(4)),
            ("wctemp", Value::U16(373)),
            ("cctemp", Value::U16(393)),
            ("power_on_hours", Value::U64(90)),
        ]);
        let h = health_from_attrs(attrs);
        assert_eq!(h.percent_used, Some(46));
        assert_eq!(h.media_errors, Some(2));
        assert_eq!(h.critical_warning, Some(4));
        assert_close(h.warn_temp_c, Some(99.85));
        assert_eq!(h.power_on_hours, Some(90));
    }

    #[test]
    fn node_paths_encode() {
        assert_eq!(node_basename("/dev/nvme0n1"), "nvme0n1");
        assert_eq!(node_basename("/dev/nvme0n1p2"), "nvme0n1p2");
        assert_eq!(node_basename("/dev/sda1"), "sda1");
        // dashes in exotic names get UDisks2-style encoding
        assert_eq!(node_basename("md-0"), "md_2d0");
    }

    #[test]
    fn profile_args() {
        assert_eq!(Profile::from_arg("balanced").unwrap(), Profile::Balanced);
        assert_eq!(
            Profile::from_arg("power_saver").unwrap(),
            Profile::PowerSaver
        );
        assert_eq!(
            Profile::from_arg("POWERSaver").unwrap(),
            Profile::PowerSaver
        );
        assert!(Profile::from_arg("turbo").is_err());
        assert_eq!(Profile::Performance.as_str(), "performance");
    }

    fn assert_close(a: Option<f64>, b: Option<f64>) {
        match (a, b) {
            (Some(x), Some(y)) => assert!((x - y).abs() < 1e-6, "{x} != {y}"),
            other => assert_eq!(other.0, other.1),
        }
    }

    #[test]
    fn decode_profiles_property() {
        // aa{sv}: [ {Profile: "balanced"} ] — same shape as the live daemon sends
        let mut dict = zbus::zvariant::Dict::new(&Signature::Str, &Signature::Variant);
        dict.append(
            Value::Str("Profile".into()),
            // variant values arrive wrapped
            Value::Value(Box::new(Value::Str("balanced".into()))),
        )
        .unwrap();
        let dict_sig = Signature::dict(Signature::Str, Signature::Variant);
        let mut arr = Array::new(&dict_sig);
        arr.append(Value::Dict(dict)).unwrap();
        let v = Value::Array(arr);
        assert_eq!(profiles_from_value(&v), vec!["balanced".to_string()]);
    }

    #[test]
    fn decode_garbage_profiles() {
        assert!(profiles_from_value(&Value::U64(5)).is_empty());
        let empty = Value::Dict(zbus::zvariant::Dict::new(
            &Signature::Str,
            &Signature::Variant,
        ));
        assert!(profiles_from_value(&empty).is_empty());
    }

    #[test]
    fn controller_smart_warnings_and_update_validity_are_preserved() {
        let mut warnings = Array::new(&Signature::Str);
        warnings.append(Value::Str("degraded".into())).unwrap();
        let mut health = NvmeHealth::default();
        apply_smart_properties(
            &mut health,
            attrs_from(vec![
                ("SmartUpdated", Value::U64(1)),
                ("SmartCriticalWarning", Value::Array(warnings)),
                ("SmartPowerOnHours", Value::U64(123)),
            ]),
        )
        .unwrap();
        assert_eq!(health.critical_warnings, Some(vec!["degraded".into()]));
        assert_eq!(health.power_on_hours, Some(123));
        assert!(
            apply_smart_properties(
                &mut health,
                attrs_from(vec![("SmartUpdated", Value::U64(0))])
            )
            .is_err()
        );
    }
}
