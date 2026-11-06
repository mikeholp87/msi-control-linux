//! Memory usage from `/proc/meminfo` and `/proc/pressure/memory`.
//!
//! "Used" follows the `MemTotal - MemAvailable` calculation (the amount that
//! would actually be allocated on demand), never "total - free", so reclaimable
//! cache is not double-labeled. All fields can be `None` when an interface
//! (e.g. PSI) is compiled out of the running kernel.

use crate::fs::SourcePaths;
use nuc_core::MemorySnapshot;
use std::path::Path;

pub struct MemoryMonitor {
    paths: SourcePaths,
}

fn parse_meminfo(p: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(p) {
        for line in text.lines() {
            if let Some((key, rest)) = line.split_once(':')
                && let Ok(v) = rest.trim().trim_end_matches(" kB").parse::<u64>()
            {
                out.push((key.to_string(), v));
            }
        }
    }
    out
}

impl MemoryMonitor {
    pub fn new(paths: SourcePaths) -> Self {
        MemoryMonitor { paths }
    }

    pub fn sample(&self) -> MemorySnapshot {
        let info: std::collections::HashMap<String, u64> =
            parse_meminfo(&self.paths.proc.join("meminfo"))
                .into_iter()
                .collect();
        let get = |k: &str| info.get(k).copied();
        let total = get("MemTotal");
        let available = get("MemAvailable");
        // Free reports truly unused pages; used accounts for reclaimable cache
        let used = match (total, available) {
            (Some(t), Some(a)) => Some(t.saturating_sub(a)),
            _ => None,
        };
        let cached = {
            // SReclaimable is cache too (slab reclaim)
            match (get("Cached"), get("SReclaimable")) {
                (Some(c), Some(s)) => Some(c + s),
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        let pressure = self.read_pressure();
        MemorySnapshot {
            total_kib: total,
            used_kib: used,
            available_kib: available,
            cached_kib: cached,
            buffers_kib: get("Buffers"),
            swap_total_kib: get("SwapTotal"),
            swap_used_kib: get("SwapTotal")
                .zip(get("SwapFree"))
                .map(|(t, f)| t.saturating_sub(f)),
            pressure,
        }
    }

    /// /proc/pressure/memory "full" average10 share as 0..1.
    fn read_pressure(&self) -> Option<f64> {
        let p = self.paths.proc.join("pressure/memory");
        let text = std::fs::read_to_string(p).ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("full")
                && let Some(avg) = rest.split_whitespace().find_map(|tok| {
                    tok.strip_prefix("avg10=")
                        .and_then(|v| v.parse::<f64>().ok())
                })
            {
                return Some((avg / 100.0).clamp(0.0, 1.0));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SourcePaths;
    use std::fs;
    use std::path::Path;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn used_is_total_minus_available() {
        let root = std::env::temp_dir().join("nuc-fix-mem");
        let _ = fs::remove_dir_all(&root);
        write(
            &root.join("proc/meminfo"),
            "MemTotal:        32000000 kB\nMemFree:          1500000 kB\nMemAvailable:    12000000 kB\nBuffers:           300000 kB\nCached:          9000000 kB\nSReclaimable:      500000 kB\nSwapTotal:       16000000 kB\nSwapFree:        15000000 kB\n",
        );
        let m = MemoryMonitor::new(SourcePaths::fixture(&root));
        let s = m.sample();
        assert_eq!(s.total_kib, Some(32000000));
        assert_eq!(s.used_kib, Some(20000000), "used = total - available");
        assert_ne!(s.used_kib, Some(s.total_kib.unwrap() - 1500000));
        assert_eq!(s.available_kib, Some(12000000));
        assert_eq!(s.cached_kib, Some(9500000));
        assert_eq!(s.swap_used_kib, Some(1000000));
        assert!(s.pressure.is_none(), "no PSI fixture");
        let _ = fs::remove_dir_all(&root);
    }
}
