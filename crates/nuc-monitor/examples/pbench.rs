//! Times each provider's sample path individually for the perf report.
//! `cargo run -p nuc-monitor --example pbench --release`

use std::cell::RefCell;

fn main() {
    let paths = nuc_monitor::fs::SourcePaths::system();
    let timed = |label: &str, f: &dyn Fn()| {
        let t = std::time::Instant::now();
        f();
        eprintln!("{label:26} {:?}", t.elapsed());
    };

    let topo = nuc_monitor::topologyparse::discover(&paths).unwrap();
    let cpu = nuc_monitor::cpu::CpuMonitor::new(paths.clone(), topo.clone());
    let thermal = nuc_monitor::thermal::ThermalMonitor::new(paths.clone());
    let fan = nuc_monitor::fan::FanMonitor::new(paths.clone());
    let mem = nuc_monitor::memory::MemoryMonitor::new(paths.clone());
    let storage = nuc_monitor::storage::StorageMonitor::new(paths.clone());
    let mut gpu = nuc_monitor::gpu::GpuMonitor::new(paths.clone(), 2000);
    gpu.discover();

    timed("cpu.sample_freq_state", &|| {
        let _ = cpu.sample_freq_state();
    });
    timed("cpu.sample_throttle", &|| {
        let _ = cpu.sample_throttle();
    });
    let cpu = RefCell::new(cpu);
    timed("cpu.sample (usage+power)", &|| {
        let _ = cpu.borrow().sample(true);
    });
    let thermal = RefCell::new(thermal);
    timed("thermal.discover", &|| {
        let _ = thermal.borrow_mut().discover();
    });
    timed("thermal.sample", &|| {
        let _ = thermal.borrow_mut().sample_temperatures();
    });
    let fan = RefCell::new(fan);
    timed("fan.sample", &|| {
        let _ = fan.borrow_mut().sample_fans();
    });
    timed("memory.sample", &|| {
        let _ = mem.sample();
    });
    timed("storage.discover", &|| {
        let _ = storage.discover();
    });
    timed("rapl energy read", &|| {
        let _ = cpu.borrow().read_package_energy_uj();
    });
    let gpu = RefCell::new(gpu);
    timed("gpu.sample", &|| {
        let _ = gpu.borrow().sample();
    });
    timed("sysinfo.discover", &|| {
        let _ = nuc_monitor::sysinfo::discover();
    });

    // whole engine for comparison (1 warm tick first)
    let mut engine = nuc_monitor::engine::Engine::build(
        &paths,
        &nuc_monitor::SamplerConfig {
            interval: std::time::Duration::from_secs(2),
            gpu_enabled: false,
            record: false,
        },
    )
    .unwrap();
    let _ = engine.sample();
    let engine = RefCell::new(engine);
    timed("engine.sample (warm)", &|| {
        let _ = engine.borrow_mut().sample();
    });
}
