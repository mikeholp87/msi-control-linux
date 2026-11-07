//! NUC Control CLI — `nuc-control`
//!
//! Read-first CLI for hardware status. Every read-only flag works without
//! touching any kernel state or launching any graphical stack; profile
//! changes go exclusively through the polkit-authorized power-profiles-daemon
//! D-Bus path and are verified by re-reading afterwards.

use nuc_core::config::Config;
use nuc_core::units::fmt_watts;
use nuc_core::{AppError, CoreKind, Snapshot};
use nuc_monitor::engine::Engine;
use nuc_monitor::fs::SourcePaths;
use std::process::ExitCode;

const USAGE: &str = "\
nuc-control — MSI mini PC hardware & power monitor (read-first)

USAGE:
  nuc-control --cpu                 CPU info, frequencies, usage, package power
  nuc-control --gpu                 GPU frequency/utilization status
  nuc-control --sensors             all temperature + fan readings
  nuc-control --temperatures        temperatures only
  nuc-control --power               power profile, EPP, turbo, RAPL zones
  nuc-control --health              NVMe SMART/health via UDisks2
  nuc-control --hardware            machine identity and capability summary
  nuc-control --diagnostics         run the read-only sanity checks
  nuc-control --export-report       write diagnostics report (Markdown + JSON)
                                    into the data dir and print the paths
  nuc-control --profile <name>      switch power profile (power-saver|balanced|
                                    performance); verified by re-read
  nuc-control --json                emit machine-readable JSON (with section flags)
  nuc-control --version             version information

Notes:
  * Unavailable values never appear as zeros — they are reported honestly.
  * No flag ever launches the GUI; the GUI is a separate binary (nuc-gui).
  * Fan and thermal protections are never changed by this program.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!(
            "nuc-control {} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_NAME")
        );
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") || args.is_empty() {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let json = args.iter().any(|a| a == "--json");

    match run(&args, json) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Flags {
    cpu: bool,
    gpu: bool,
    sensors: bool,
    temperatures: bool,
    power: bool,
    health: bool,
    hardware: bool,
    diagnostics: bool,
    export_report: bool,
}

fn parse_args(args: &[String]) -> Result<(Flags, Option<String>), AppError> {
    let mut flags = Flags::default();
    let mut profile: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cpu" => flags.cpu = true,
            "--gpu" => flags.gpu = true,
            "--sensors" => flags.sensors = true,
            "--temperatures" => flags.temperatures = true,
            "--power" => flags.power = true,
            "--health" => flags.health = true,
            "--hardware" => flags.hardware = true,
            "--diagnostics" => flags.diagnostics = true,
            "--export-report" => flags.export_report = true,
            "--profile" => {
                i += 1;
                profile = Some(
                    args.get(i)
                        .ok_or_else(|| AppError::Parse {
                            context: "args".into(),
                            message: "--profile requires an argument".into(),
                        })?
                        .clone(),
                );
            }
            "--json" => {}
            other => {
                return Err(AppError::Parse {
                    context: "args".into(),
                    message: format!("unknown argument '{other}' (see --help)"),
                });
            }
        }
        i += 1;
    }

    let sections = flags.cpu
        || flags.gpu
        || flags.sensors
        || flags.temperatures
        || flags.power
        || flags.health
        || flags.hardware;
    let modes = usize::from(profile.is_some())
        + usize::from(flags.diagnostics)
        + usize::from(flags.export_report);
    if modes > 1 || (modes > 0 && sections) {
        return Err(AppError::Parse {
            context: "args".into(),
            message:
                "--profile, --diagnostics and --export-report each run alone; --json is allowed"
                    .into(),
        });
    }
    if let Some(wanted) = &profile {
        nuc_power::Profile::from_arg(wanted)?;
    }
    Ok((flags, profile))
}

fn run(args: &[String], json: bool) -> Result<ExitCode, AppError> {
    let (flags, profile) = parse_args(args)?;

    // Genuine writes are dispatched first and run alone.
    if let Some(wanted) = profile {
        let client = nuc_power::ProfileClient::new()?;
        let state = client.set_profile_arg(&wanted)?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&state).map_err(json_err)?
            );
        } else {
            println!(
                "active profile: {}",
                state.active.clone().unwrap_or_else(|| "unavailable".into())
            );
            println!("available: {:?}", state.profiles);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (config, warning) = Config::load();
    if let Some(w) = warning {
        eprintln!("warning: {w}");
    }

    if flags.export_report {
        return export_report(&config, json);
    }
    if flags.diagnostics {
        return run_diagnostics(&config, json);
    }
    if !(flags.cpu
        || flags.gpu
        || flags.sensors
        || flags.temperatures
        || flags.power
        || flags.health
        || flags.hardware)
    {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }

    // Read-only one-shot sample (two ticks so delta metrics have a baseline).
    let paths = SourcePaths::system();
    let mut engine = Engine::build(
        &paths,
        &nuc_monitor::SamplerConfig {
            interval: std::time::Duration::from_secs(2),
            gpu_enabled: config.monitoring.gpu_enabled,
            record: false,
        },
    )?;
    let _ = engine.sample();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let mut snap = engine.sample();
    if snap.profile.is_none() {
        snap.profile = nuc_power::ProfileClient::new()
            .ok()
            .and_then(|c| c.state().ok());
    }

    let devices = if flags.health {
        Some(health_device_list()?)
    } else {
        None
    };
    let info = if flags.hardware {
        Some(nuc_monitor::sysinfo::discover())
    } else {
        None
    };

    if json {
        let mut obj = serde_json::Map::new();
        if flags.cpu {
            obj.insert("cpu".into(), cpu_value(&snap));
        }
        if flags.gpu {
            obj.insert("gpu".into(), gpu_value(&snap));
        }
        if flags.temperatures {
            obj.insert("temperatures".into(), temps_value(&snap));
        }
        if flags.sensors {
            obj.insert(
                "sensors".into(),
                serde_json::json!({ "temperatures": temps_value(&snap), "fans": fans_value(&snap) }),
            );
        }
        if flags.power {
            obj.insert("power".into(), power_value(&snap));
        }
        if let Some(d) = &devices {
            obj.insert("health".into(), serde_json::Value::Array(d.clone()));
        }
        if let Some(info) = &info {
            obj.insert(
                "hardware".into(),
                serde_json::to_value(info).map_err(json_err)?,
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(obj)).map_err(json_err)?
        );
    } else {
        if flags.cpu {
            cpu_text(&snap);
        }
        if flags.gpu {
            gpu_text(&snap);
        }
        if flags.temperatures || flags.sensors {
            temps_text(&snap);
        }
        if flags.sensors {
            fans_text(&snap);
        }
        if flags.power {
            power_text(&snap);
        }
        if let Some(d) = &devices {
            health_text(d);
        }
        if let Some(info) = &info {
            hardware_text(info);
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------ text render

struct Section {
    title: String,
    lines: Vec<String>,
}

impl Section {
    fn new(title: &str) -> Section {
        Section {
            title: title.into(),
            lines: Vec::new(),
        }
    }
    fn push(&mut self, key: impl std::fmt::Display, value: impl std::fmt::Display) {
        self.lines.push(format!("{key:24} : {value}"));
    }
    fn note(&mut self, text: impl std::fmt::Display) {
        self.lines.push(format!("- {text}"));
    }
}

fn emit(sec: Section) {
    println!("== {}", sec.title);
    for line in sec.lines {
        println!("{line}");
    }
    println!();
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string())
        .unwrap_or_else(|| "Unavailable".into())
}

fn cpu_text(snap: &Snapshot) {
    let mut sec = Section::new("CPU");
    let c = &snap.cpu;
    sec.push(
        "model",
        if c.info.model.is_empty() {
            "Unavailable".to_string()
        } else {
            c.info.model.clone()
        },
    );
    sec.push("logical cpus", c.topology.logical_cpus);
    sec.push(
        "cores (P/E)",
        format!(
            "{}/{} of {}",
            c.topology.count(CoreKind::Performance),
            c.topology.count(CoreKind::Efficient),
            c.topology.physical_cores()
        ),
    );
    if !c.topology.classification_basis.is_empty() {
        sec.push(
            "classification basis",
            c.topology.classification_basis.clone(),
        );
    }
    sec.push(
        "usage (total)",
        c.usage_total
            .map(|u| format!("{u:.1}%"))
            .unwrap_or_else(|| "Unavailable".into()),
    );
    sec.push(
        "freq gov",
        c.freq
            .governor
            .clone()
            .unwrap_or_else(|| "Unavailable".into()),
    );
    sec.push(
        "EPP",
        c.freq.epp.clone().unwrap_or_else(|| "Unavailable".into()),
    );
    sec.push(
        "turbo",
        match c.freq.turbo_enabled {
            Some(true) => "enabled",
            Some(false) => "disabled",
            None => "Unavailable",
        },
    );
    let per_policy_hz = c
        .freq
        .policies
        .iter()
        .map(|p| {
            format!(
                "{}: cur {} kHz (min {} / max {})",
                p.id,
                opt(p.f_cur_khz),
                opt(p.f_min_khz),
                opt(p.f_max_khz)
            )
        })
        .collect::<Vec<_>>();
    if !per_policy_hz.is_empty() {
        sec.push("freq policies", per_policy_hz.join(" · "));
    }
    sec.push(
        "package power (interval)",
        c.interval_power_w
            .map(fmt_watts)
            .unwrap_or_else(|| "Unavailable".into()),
    );
    sec.push(
        "session peak package power",
        snap.peak_power_w
            .map(fmt_watts)
            .unwrap_or_else(|| "Unavailable".into()),
    );
    sec.push(
        "throttle counters",
        format!(
            "core {} · package {}",
            opt(c.throttle.core_throttle_count),
            opt(c.throttle.package_throttle_count),
        ),
    );
    emit(sec);
}

fn gpu_text(snap: &Snapshot) {
    let mut sec = Section::new("GPU");
    match &snap.gpu {
        Some(g) => {
            sec.push("id", g.id.clone());
            sec.push("driver", g.driver.clone());
            sec.push(
                "freq act/max/rp0 (MHz)",
                format!(
                    "{}/{}/{}",
                    opt(g.act_freq_mhz),
                    opt(g.max_freq_mhz),
                    opt(g.rp0_freq_mhz)
                ),
            );
            sec.push(
                "utilization",
                g.utilization_pct
                    .map(|u| format!("{u:.0}%"))
                    .unwrap_or_else(|| format!("Unavailable ({})", g.utilization_source)),
            );
        }
        None => sec.note("GPU snapshot unavailable (disabled in config or not detected)"),
    }
    emit(sec);
}

fn temps_text(snap: &Snapshot) {
    let mut sec = Section::new("Temperatures");
    if snap.temperatures.is_empty() {
        sec.note("no temperature sensors found");
    }
    for t in &snap.temperatures {
        let value = t
            .value_c
            .map(|v| format!("{v:.1} °C"))
            .unwrap_or_else(|| "Unavailable".into());
        let limits = match (t.warn_c, t.crit_c) {
            (Some(w), Some(c)) => format!(" (warn {w:.0} / crit {c:.0} °C)"),
            (Some(w), None) => format!(" (warn {w:.0} °C)"),
            (None, Some(c)) => format!(" (crit {c:.0} °C)"),
            _ => String::new(),
        };
        sec.push(
            t.label.to_string(),
            format!("{value}{limits}  [{}]", t.source),
        );
    }
    emit(sec);
}

fn fans_text(snap: &Snapshot) {
    let mut sec = Section::new("Fans");
    if snap.fans.is_empty() {
        sec.note("no fan sensors found (no fan control exists on this platform)");
    }
    for f in &snap.fans {
        let rpm = f.formatted_speed();
        let fault = match f.fault {
            Some(true) => "  [FAULT]",
            Some(false) => "",
            None => "",
        };
        sec.push(f.label.to_string(), format!("{rpm}{fault}  [{}]", f.source));
    }
    emit(sec);
}

fn power_text(snap: &Snapshot) {
    let mut sec = Section::new("Power & policy");
    match &snap.profile {
        Some(p) => {
            sec.push(
                "active profile",
                p.active.clone().unwrap_or_else(|| "Unavailable".into()),
            );
            sec.push("available profiles", format!("{:?}", p.profiles));
            if p.degraded {
                sec.note(format!(
                    "profile degraded: {}",
                    p.degraded_reason
                        .clone()
                        .unwrap_or_else(|| "cause unavailable".into())
                ));
            }
        }
        None => sec.note("power-profiles-daemon state unavailable"),
    }
    if snap.rapl.is_empty() {
        sec.note("no RAPL zones readable — per-domain power unavailable");
    }
    for z in &snap.rapl {
        let power = z
            .power_w
            .map(fmt_watts)
            .unwrap_or_else(|| "Unavailable".into());
        sec.push(z.id.clone(), format!("{}: {power}", z.name));
    }
    emit(sec);
}

fn health_text(devices: &[serde_json::Value]) {
    let mut sec = Section::new("Device health");
    if devices.is_empty() {
        sec.note("no storage devices detected");
    }
    for dev in devices {
        let node = dev
            .get("node")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        sec.push(
            node.clone(),
            format!(
                "{} — {}, SMART {}",
                dev.get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                dev.get("transport").and_then(|v| v.as_str()).unwrap_or("?"),
                if dev.get("smart").is_some() {
                    "below"
                } else {
                    "unavailable"
                }
            ),
        );
        if let Some(h) = dev.get("smart") {
            sec.push("  percent_used", num_of(h.get("percent_used")));
            sec.push("  avail_spare", num_of(h.get("avail_spare_pct")));
            sec.push("  power_on_hours", num_of(h.get("power_on_hours")));
            sec.push("  unsafe_shutdowns", num_of(h.get("unsafe_shutdowns")));
            sec.push("  media_errors", num_of(h.get("media_errors")));
            sec.push("  critical_warning", num_of(h.get("critical_warning")));
            sec.push(
                "  controller_warnings",
                h.get("critical_warnings")
                    .and_then(|v| v.as_array())
                    .map(|values| {
                        if values.is_empty() {
                            "none".into()
                        } else {
                            values
                                .iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    })
                    .unwrap_or_else(|| "Unavailable".into()),
            );
            sec.push(
                "  warn/crit temp",
                opt_pair(h.get("warn_temp_c"), h.get("crit_temp_c")),
            );
            sec.push(
                "  source",
                h.get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unavailable")
                    .to_string(),
            );
        } else if let Some(err) = dev.get("smart_error") {
            sec.note(format!("{node}: SMART unavailable — {err}"));
        }
    }
    emit(sec);
}

fn hardware_text(info: &nuc_monitor::sysinfo::SystemInfo) {
    let mut sec = Section::new("Hardware identity");
    let id = &info.identity;
    sec.push(
        "vendor / product",
        format!("{} / {}", id.vendor, id.product),
    );
    sec.push("board", id.board.clone());
    sec.push(
        "BIOS",
        format!("{} {} ({})", id.bios_vendor, id.bios_version, id.bios_date),
    );
    sec.push("kernel", info.kernel.clone());
    sec.push("distro", info.distro.clone());
    sec.push(
        "session",
        format!("{} ({})", info.session_type, info.desktop_env),
    );
    emit(sec);
}

// ------------------------------------------------------------ json helpers

fn num_of(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "Unavailable".into(),
    }
}

fn json_err(e: serde_json::Error) -> AppError {
    AppError::Other(format!("json: {e}"))
}

fn opt_pair(a: Option<&serde_json::Value>, b: Option<&serde_json::Value>) -> String {
    let fmt = |v: Option<&serde_json::Value>| -> Option<String> {
        v.and_then(|x| x.as_f64()).map(|x| format!("{x:.1}"))
    };
    let mut parts = Vec::new();
    if let Some(x) = fmt(a) {
        parts.push(format!("warn {x} °C"));
    }
    if let Some(x) = fmt(b) {
        parts.push(format!("crit {x} °C"));
    }
    if parts.is_empty() {
        "Unavailable".into()
    } else {
        parts.join(" / ")
    }
}

fn cpu_value(snap: &Snapshot) -> serde_json::Value {
    let c = &snap.cpu;
    serde_json::json!({
        "model": c.info.model,
        "topology": {
            "logical_cpus": c.topology.logical_cpus,
            "physical_cores": c.topology.physical_cores(),
            "p_cores": c.topology.count(CoreKind::Performance),
            "e_cores": c.topology.count(CoreKind::Efficient),
            "basis": c.topology.classification_basis,
        },
        "usage_total": c.usage_total,
        "interval_power_w": c.interval_power_w,
        "session_peak_power_w": snap.peak_power_w,
        "driver": c.freq.running_driver,
        "governor": c.freq.governor,
        "epp": c.freq.epp,
        "turbo_enabled": c.freq.turbo_enabled,
        "throttle": serde_json::to_value(&c.throttle).ok(),
    })
}

fn gpu_value(snap: &Snapshot) -> serde_json::Value {
    match &snap.gpu {
        Some(g) => serde_json::to_value(g).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

fn temps_value(snap: &Snapshot) -> serde_json::Value {
    serde_json::to_value(&snap.temperatures).unwrap_or(serde_json::Value::Null)
}

fn fans_value(snap: &Snapshot) -> serde_json::Value {
    serde_json::to_value(&snap.fans).unwrap_or(serde_json::Value::Null)
}

fn power_value(snap: &Snapshot) -> serde_json::Value {
    serde_json::json!({
        "profile": snap.profile.as_ref().and_then(|p| serde_json::to_value(p).ok()),
        "rapl": serde_json::to_value(&snap.rapl).unwrap_or(serde_json::Value::Null),
    })
}

// ------------------------------------------------------------ reports

fn run_diagnostics(config: &Config, json: bool) -> Result<ExitCode, AppError> {
    let paths = SourcePaths::system();
    let report = nuc_diagnostics::run(&paths, config, "live")?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(json_err)?
        );
    } else {
        for c in &report.checks {
            println!(
                "[{:11}] {:14} {:<22} {}",
                c.status.as_str(),
                c.category,
                c.id,
                c.detail
            );
        }
        println!("\nverdict: {}", report.verdict());
    }
    let fails = report.status_count(nuc_diagnostics::Status::Fail);
    Ok(if fails > 0 {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

fn export_report(config: &Config, json: bool) -> Result<ExitCode, AppError> {
    let paths = SourcePaths::system();
    let report = nuc_diagnostics::run(&paths, config, "live")?;
    let dir = nuc_core::config::data_dir();
    std::fs::create_dir_all(&dir)?;
    let md_path = dir.join("diagnostics-report.md");
    let js_path = dir.join("diagnostics-report.json");
    std::fs::write(&md_path, report.export_markdown())?;
    std::fs::write(
        &js_path,
        serde_json::to_string_pretty(&report).map_err(json_err)?,
    )?;
    if json {
        println!(
            "{}",
            serde_json::json!({"markdown":md_path.display().to_string(), "json":js_path.display().to_string(), "verdict":report.verdict()})
        );
    } else {
        println!("markdown : {}", md_path.display());
        println!("json     : {}", js_path.display());
        println!("verdict  : {}", report.verdict());
    }
    Ok(ExitCode::SUCCESS)
}

/// NVMe/block health via UDisks2 for every discovered storage device.
fn health_device_list() -> Result<Vec<serde_json::Value>, AppError> {
    let paths = SourcePaths::system();
    let mut engine = Engine::build(
        &paths,
        &nuc_monitor::SamplerConfig {
            interval: std::time::Duration::from_secs(2),
            gpu_enabled: false,
            record: false,
        },
    )?;
    let snap = engine.sample();
    let mut list = Vec::new();
    for d in &snap.storage {
        let mut v = serde_json::json!({
            "node": d.node,
            "model": d.model,
            "transport": d.transport,
            "size_bytes": d.size_bytes,
            "temperature_c": d.temp_c,
        });
        match nuc_power::nvme_health(&d.node) {
            Ok(h) => v["smart"] = serde_json::to_value(&h).map_err(json_err)?,
            Err(e) => v["smart_error"] = serde_json::json!(e.to_string()),
        }
        list.push(v);
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn writes_and_reports_reject_conflicting_sections_before_dbus() {
        for values in [
            &["--profile", "balanced", "--cpu"][..],
            &["--diagnostics", "--health"],
            &["--export-report", "--diagnostics"],
            &["--profile", "garbage"],
        ] {
            assert!(parse_args(&args(values)).is_err());
        }
        assert!(parse_args(&args(&["--profile", "balanced", "--json"])).is_ok());
        assert!(parse_args(&args(&["--export-report", "--json"])).is_ok());
        assert!(parse_args(&args(&["--cpu", "--gpu", "--json"])).is_ok());
    }
}
