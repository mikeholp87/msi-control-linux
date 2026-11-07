//! Live verification of nuc-power against the running system bus.
//! Read-only by default; pass `--switch <profile>` to exercise a profile
//! change (polkit-authorized, verified by re-reading afterwards).

fn main() {
    let args: Vec<String> = std::env::args().collect();

    println!("== power-profiles-daemon state");
    match nuc_power::ProfileClient::new() {
        Ok(client) => match client.state() {
            Ok(s) => {
                println!("backend   : {}", s.backend);
                println!(
                    "active    : {}",
                    s.active.as_deref().unwrap_or("Unavailable")
                );
                println!("profiles  : {:?}", s.profiles);
                println!(
                    "degraded  : {} ({})",
                    s.degraded,
                    s.degraded_reason.as_deref().unwrap_or("none")
                );
                if let Some(sub) = args
                    .iter()
                    .position(|a| a == "--switch")
                    .and_then(|i| args.get(i + 1))
                {
                    println!("--switch {sub}");
                    match client.set_profile_arg(sub) {
                        Ok(after) => {
                            println!(
                                "switched  : active now {}",
                                after.active.as_deref().unwrap_or("Unavailable")
                            );
                        }
                        Err(e) => println!("switch FAILED: {e}"),
                    }
                }
            }
            Err(e) => println!("state: ERROR {e}"),
        },
        Err(e) => println!("client: ERROR {e}"),
    }

    println!("\n== NVMe health via UDisks2 (nvme0n1)");
    match nuc_power::nvme_health("/dev/nvme0n1") {
        Ok(h) => {
            println!("percent_used      : {}", opt_u8(h.percent_used));
            println!(
                "avail_spare       : {} (threshold {})",
                opt_u8(h.avail_spare_pct),
                opt_u8(h.spare_threshold_pct)
            );
            println!("units_written     : {}", opt_u64(h.units_written));
            println!("units_read        : {}", opt_u64(h.units_read));
            println!("power_on_hours    : {}", opt_u64(h.power_on_hours));
            println!("unsafe_shutdowns  : {}", opt_u64(h.unsafe_shutdowns));
            println!("media_errors      : {}", opt_u64(h.media_errors));
            println!("error_log_entries : {}", opt_u64(h.error_log_entries));
            println!("critical_warning  : {}", opt_u64(h.critical_warning));
            println!(
                "wctemp/cctemp     : {} / {}",
                opt_f(h.warn_temp_c),
                opt_f(h.crit_temp_c)
            );
            println!("source            : {}", h.source);
        }
        Err(e) => println!("SMART: ERROR {e}"),
    }

    // A non-NVMe device must be reported honestly as unsupported.
    println!("\n== NVMe health on non-NVMe device (sda, expected error)");
    match nuc_power::nvme_health("/dev/sda") {
        Ok(_) => println!("UNEXPECTED: sda reported NVMe data"),
        Err(e) => println!("correctly refused: {e}"),
    }

    println!("\n== conflict scan (read-only unit checks)");
    for (name, state) in nuc_power::conflict_scan() {
        println!("{name:55} {state}");
    }
}

fn opt_u8(v: Option<u8>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "Unavailable".into(),
    }
}
fn opt_u64(v: Option<u64>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "Unavailable".into(),
    }
}
fn opt_f(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.1} °C"),
        None => "Unavailable".into(),
    }
}
