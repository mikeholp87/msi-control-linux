//! Live verification of nuc-diagnostics against the running machine.
//! Usage: `cargo run -p nuc-diagnostics --example dxlive [-- --json|--md]`

fn main() {
    let flag = std::env::args().nth(1).unwrap_or_default();
    let paths = nuc_monitor::fs::SourcePaths::system();
    let config = nuc_core::config::Config::default();
    match nuc_diagnostics::run(&paths, &config, "live") {
        Ok(report) if flag == "--md" => print!("{}", report.export_markdown()),
        Ok(report) if flag == "--json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_default()
            )
        }
        Ok(report) => {
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
        Err(e) => eprintln!("diagnostics failed: {e}"),
    }
}
