//! Quick real-hardware validation harness (not shipped).
use nuc_monitor::engine::{Engine, SamplerConfig};
use nuc_monitor::fs::SourcePaths;
use std::time::Duration;

#[allow(clippy::cast_possible_truncation)] // example output only
fn main() {
    let paths = SourcePaths::system();
    let mut engine = Engine::build(
        &paths,
        &SamplerConfig {
            interval: Duration::from_millis(1000),
            gpu_enabled: true,
            record: false,
        },
    )
    .expect("engine");
    // warm-up + two deltas
    let _ = engine.sample();
    std::thread::sleep(Duration::from_millis(1200));
    let snap = engine.sample();
    println!(
        "ts {}\ncpu_total {:?}  per-core {:?}",
        snap.ts_ms,
        snap.cpu.usage_total,
        snap.cpu
            .usage
            .iter()
            .map(|v| v.map(|f| f.round().clamp(0.0, 1e12) as i64))
            .collect::<Vec<_>>()
    );
    println!(
        "driver {} governor {:?} epp {:?} turbo {:?} policies {}",
        snap.cpu.freq.running_driver,
        snap.cpu.freq.governor,
        snap.cpu.freq.epp,
        snap.cpu.freq.turbo_enabled,
        snap.cpu.freq.policies.len()
    );
    println!(
        "throttle core={:?} pkg={:?} new core {:?} pkg {:?}",
        snap.cpu.throttle.core_throttle_count,
        snap.cpu.throttle.package_throttle_count,
        snap.cpu.new_core_throttles,
        snap.cpu.new_package_throttles
    );
    println!(
        "package power: current {:?} peak {:?}",
        snap.cpu.package_power_w, snap.peak_power_w
    );
    for t in &snap.temperatures {
        println!(
            "temp {} [{}] = {:?} crit {:?}",
            t.id, t.label, t.value_c, t.crit_c
        );
    }
    for f in &snap.fans {
        println!("fan {} = {:?} (source {})", f.label, f.rpm, f.source);
    }
    for r in &snap.rapl {
        println!(
            "rapl {} {} energy {:?} power {:?}",
            r.id, r.name, r.energy_uj, r.power_w
        );
    }
    println!(
        "mem total={:?} used={:?} avail={:?} swap_used={:?}",
        snap.memory.total_kib.map(|v| v / 1024),
        snap.memory.used_kib.map(|v| v / 1024),
        snap.memory.available_kib.map(|v| v / 1024),
        snap.memory.swap_used_kib.map(|v| v / 1024)
    );
    if let Some(g) = snap.gpu {
        println!(
            "gpu driver {} freq {:?} max {:?} util {:?}",
            g.driver, g.freq_mhz, g.max_freq_mhz, g.utilization_pct
        );
    }
    for d in &snap.storage {
        println!(
            "disk {} model {:?} {} temp {:?}",
            d.node,
            d.model,
            nuc_core::fmt_bytes(d.size_bytes.unwrap_or(0)),
            d.temp_c
        );
    }
}
