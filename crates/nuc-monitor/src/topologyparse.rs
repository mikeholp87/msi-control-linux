//! CPU topology discovery and hybrid core classification, based strictly on
//! documented kernel interfaces (`topology/*`, `cpu_capacity`).
//!
//! Classification basis is always reported back verbatim — the UI never shows
//! a P/E split unless it can be justified from kernel data.

use crate::fs::{self, SourcePaths};
use nuc_core::{AppError, CoreKind, CoreTopology, Topology};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn cpu_index(name: &str) -> Option<u32> {
    name.strip_prefix("cpu").and_then(|s| s.parse::<u32>().ok())
}

pub fn discover(paths: &SourcePaths) -> nuc_core::Result<Topology> {
    let dir = paths.cpu.as_path();
    let mut entries: Vec<(u32, PathBuf)> = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if let Some(i) = cpu_index(e.file_name().to_string_lossy().as_ref()) {
            // only offline-able attribute indicates real cpu dir
            if e.path().join("topology").is_dir() {
                entries.push((i, e.path()));
            }
        }
    }
    if entries.is_empty() {
        return Err(AppError::Unsupported(
            "no CPU topology entries found".into(),
        ));
    }
    entries.sort();

    // group threads by (package, core_id)
    let mut cores: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    let mut capacities: BTreeMap<u32, u64> = BTreeMap::new();
    for (idx, path) in &entries {
        let topo = path.join("topology");
        let package = fs::read_u64_opt(&topo.join("physical_package_id")).unwrap_or(0) as u32;
        let core_id = fs::read_u64_opt(&topo.join("core_id")).unwrap_or(*idx as u64) as u32;
        cores.entry((package, core_id)).or_default().push(*idx);
        if let Some(cap) = fs::read_u64_opt(&path.join("cpu_capacity")) {
            capacities.insert(*idx, cap);
        }
    }

    let n_cores = cores.len();
    // sibling-count must be mixed (some double-threaded, some single) for a hybrid split
    let thread_counts: Vec<usize> = cores.values().map(|v| v.len()).collect();
    let min_t = thread_counts.iter().min().copied().unwrap_or(1);
    let max_t = thread_counts.iter().max().copied().unwrap_or(1);
    let mixed_threads = max_t != min_t;

    // capacity tiers (cpu_capacity): higher tier = performance cores
    let caps: Vec<u64> = capacities.values().copied().collect();
    let mut max_cap = caps.iter().max().copied();
    let mut min_cap = caps.iter().min().copied();
    if let (Some(max), Some(min)) = (max_cap, min_cap)
        && max == min
    {
        max_cap = None;
        min_cap = None;
    }

    let mut top = Topology {
        logical_cpus: entries.len() as u32,
        cores: Vec::with_capacity(n_cores),
        classification_basis: String::new(),
    };

    for (next_order, ((_package, _core_id), mut threads)) in cores.into_iter().enumerate() {
        let next_order = next_order as u32;
        threads.sort();
        let capacity = capacities.get(&threads[0]).copied();
        let kind = if mixed_threads {
            if threads.len() > 1 {
                CoreKind::Performance
            } else {
                CoreKind::Efficient
            }
        } else if let (Some(max), Some(cap_here)) = (max_cap, capacity) {
            // capacity-differentiated
            if cap_here == max {
                CoreKind::Performance
            } else if cap_here < max {
                CoreKind::Efficient
            } else {
                CoreKind::Unknown
            }
        } else {
            CoreKind::Unknown
        };

        top.cores.push(CoreTopology {
            core_id: next_order, // monotonic order for stable display
            threads,
            kind,
            capacity,
        });
    }

    // Report how the classification was done
    let max_cap_u = max_cap.unwrap_or(0);
    top.classification_basis = if mixed_threads {
        "kernel thread siblings (cores with 2 SMT threads marked performance)".into()
    } else if max_cap_u > 0 {
        format!(
            "cpu_capacity tiers (max={max_cap_u}, min={})",
            min_cap.unwrap_or(0)
        )
    } else {
        "not determined: no mixed SMT siblings or distinct cpu_capacity tiers".into()
    };

    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(p: &Path, content: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn make_fixture(root: &Path, pattern: &[(u32, u32, Vec<u32>, Option<u64>)]) -> SourcePaths {
        let sys = root.join("sys");
        for (cpu, core_id, threads, cap) in pattern {
            // every SMT sibling gets its own topology entries sharing core_id
            for t in threads {
                let base = sys.join("devices/system/cpu").join(format!("cpu{t}"));
                let topo = base.join("topology");
                write(&topo.join("physical_package_id"), "0\n");
                write(&topo.join("core_id"), &format!("{core_id}\n"));
                write(
                    &topo.join("thread_siblings_list"),
                    &format!(
                        "{}\n",
                        threads
                            .iter()
                            .map(|x| x.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                );
                if let Some(c) = cap {
                    write(&base.join("cpu_capacity"), &format!("{c}\n"));
                }
            }
            let _ = cpu;
            // cpu_capacity belongs to the lead thread
            let _ = threads;
        }
        let paths = SourcePaths::fixture(root);
        fs::create_dir_all(root).unwrap();
        write(&sys.join("devices/system/cpu/online"), "0-11");
        paths
    }

    #[test]
    fn hybrid_from_mixed_threads() {
        let tmp = std::env::temp_dir().join("nuc-fix-topo-hybrid");
        let _ = fs::remove_dir_all(&tmp);
        let paths = make_fixture(
            &tmp,
            &[
                (0, 0, vec![0, 1], Some(1024)),
                (2, 1, vec![2, 3], Some(1024)),
                (4, 2, vec![4], Some(1024)),
                (6, 3, vec![6], Some(1024)),
            ],
        );
        let topo = discover(&paths).unwrap();
        eprintln!("DBG basis: {}", topo.classification_basis);
        eprintln!("DBG per-core: {:?}", topo.cores);
        assert_eq!(topo.logical_cpus, 6); // 2 P-cores x SMT + 2 E-cores
        assert_eq!(topo.count(CoreKind::Performance), 2);
        assert_eq!(topo.count(CoreKind::Efficient), 2);
        assert!(topo.classification_basis.contains("thread siblings"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn uniform_threads_are_unknown() {
        let tmp = std::env::temp_dir().join("nuc-fix-topo-uniform");
        let _ = fs::remove_dir_all(&tmp);
        let paths = make_fixture(
            &tmp,
            &[
                (0, 0, vec![0, 1], Some(1024)),
                (2, 1, vec![2, 3], Some(1024)),
            ],
        );
        let topo = discover(&paths).unwrap();
        assert_eq!(topo.count(CoreKind::Unknown), 2);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sparse_cpu_ids_use_their_own_capacity_and_missing_is_unknown() {
        let tmp = std::env::temp_dir().join("nuc-topo-sparse");
        let _ = fs::remove_dir_all(&tmp);
        let paths = make_fixture(
            &tmp,
            &[
                (2, 0, vec![2], Some(1024)),
                (7, 1, vec![7], Some(512)),
                (9, 2, vec![9], None),
            ],
        );
        let topology = discover(&paths).unwrap();
        assert_eq!(topology.cores[0].capacity, Some(1024));
        assert_eq!(topology.cores[0].kind, CoreKind::Performance);
        assert_eq!(topology.cores[1].kind, CoreKind::Efficient);
        assert_eq!(topology.cores[2].kind, CoreKind::Unknown);
        fs::remove_dir_all(tmp).unwrap();
    }
}
