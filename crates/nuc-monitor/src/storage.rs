//! Block device discovery from `/sys/class/block` with NVMe temperature
//! from the nvme hwmon chip. NVMe SMART attributes come through UDisks2 D-Bus
//! (injected by the `nuc-power` dbus client) because `/dev/nvme*` requires
//! special privileges for the unprivileged user. Health data is provided
//! with a source label and interpreted conservatively.

use crate::fs::{self, SourcePaths};
use nuc_core::{NvmeHealth, PartitionInfo, StorageDevice};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct StorageMonitor {
    paths: SourcePaths,
    /// Injected drive-key -> NVMe health from an authorized backend (UDisks2).
    health: std::sync::Mutex<HashMap<String, NvmeHealth>>,
}

impl StorageMonitor {
    pub fn new(paths: SourcePaths) -> Self {
        StorageMonitor {
            paths,
            health: std::sync::Mutex::default(),
        }
    }

    /// Replace the injected health map (call after D-Bus refresh).
    pub fn inject_health(&self, map: HashMap<String, NvmeHealth>) {
        *self.health.lock().unwrap() = map;
    }

    pub fn discover(&self) -> Vec<StorageDevice> {
        let mut candidates: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.paths.class_block) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                candidates.push((name.clone(), e.path()));
                let _ = name;
            }
        }
        // Whole disks: a name that is not a derived partition of any other
        // top-level entry (partition names extend a device's name).
        let mut drives: Vec<(String, PathBuf)> = Vec::new();
        let mut virtuals: Vec<String> = Vec::new();
        for (name, _path) in &candidates {
            let is_virtual = name.starts_with("zram")
                || name.starts_with("ram")
                || name.starts_with("loop")
                || name.starts_with("dm-")
                || name.starts_with("md");
            if is_virtual {
                virtuals.push(name.clone());
            }
        }
        for (name, path) in &candidates {
            if virtuals.iter().any(|v| name.starts_with(v.as_str())) {
                continue;
            }
            let is_partition = path.join("partition").exists();
            if is_partition {
                continue;
            }
            drives.push((name.clone(), path.clone()));
        }
        drives.sort();
        drives
            .into_iter()
            .map(|(name, path)| self.read_device(&name, &path))
            .collect()
    }

    fn read_device(&self, name: &str, path: &Path) -> StorageDevice {
        let model = fs::read_string_opt(&path.join("device/model")).map(|s| s.trim().to_string());
        let size_sectors = fs::read_u64_opt(&path.join("size"));
        let rotational = fs::read_string_opt(&path.join("queue/rotational"))
            .map(|v| v == "1")
            .unwrap_or(false);
        let transport = detect_transport(name, path);
        let firmware = fs::read_string_opt(&path.join("device/firmware_rev"))
            .or_else(|| fs::read_string_opt(&path.join("device/rev")))
            .map(|s| s.trim().to_string());
        let partitions = self.read_partitions(name, path);
        let (temp_c, temp_crit) = self.nvme_temp(name, path);

        let health_key = format!("nvme:{name}");
        let nvme_health = self
            .health
            .lock()
            .ok()
            .and_then(|h| h.get(&health_key).cloned());

        StorageDevice {
            node: format!("/dev/{name}"),
            model,
            size_bytes: size_sectors.map(|s| s * 512),
            transport,
            rotational,
            firmware,
            temp_c,
            temp_crit_c: temp_crit,
            partitions,
            nvme_health,
        }
    }

    fn read_partitions(&self, disk: &str, path: &Path) -> Vec<PartitionInfo> {
        let mut parts = Vec::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for e in entries.flatten() {
                let pname = e.file_name().to_string_lossy().into_owned();
                if !pname.starts_with(disk) || !e.path().join("partition").exists() {
                    continue;
                }
                let size = fs::read_u64_opt(&e.path().join("size")).map(|s| s * 512);
                let (mount, fstype) = find_mount(&self.paths.proc, &format!("/dev/{pname}"));
                parts.push(PartitionInfo {
                    node: format!("/dev/{pname}"),
                    size_bytes: size,
                    mountpoint: mount,
                    fstype,
                });
            }
        }
        parts.sort_by(|a, b| a.node.cmp(&b.node));
        parts.truncate(32);
        parts
    }

    /// NVMe temperature from the nvme hwmon chip, if this is an NVMe device.
    fn nvme_temp(&self, disk: &str, path: &Path) -> (Option<f64>, Option<f64>) {
        if !disk.starts_with("nvme") {
            return (None, None);
        }
        let Ok(device) = std::fs::canonicalize(path.join("device")) else {
            return (None, None);
        };
        if let Ok(entries) = std::fs::read_dir(&self.paths.class_hwmon) {
            for e in entries.flatten() {
                let dir = e.path();
                if fs::read_string_opt(&dir.join("name")).as_deref() == Some("nvme")
                    && std::fs::canonicalize(dir.join("device")).ok().as_ref() == Some(&device)
                    && let Some(raw) = fs::read_i64_opt(&dir.join("temp1_input"))
                {
                    let crit = fs::read_i64_opt(&dir.join("temp1_crit"))
                        .map(nuc_core::millidegree_to_celsius);
                    return (Some(nuc_core::millidegree_to_celsius(raw)), crit);
                }
            }
        }
        (None, None)
    }
}

fn detect_transport(name: &str, path: &Path) -> String {
    if name.starts_with("nvme") {
        return "NVMe".into();
    }
    // walk device symlinks up for hints
    let device = std::fs::canonicalize(path.join("device")).unwrap_or_default();
    let s = device.to_string_lossy();
    if s.contains("/usb") {
        "USB".into()
    } else if s.contains("/ata") || s.contains("ahci") {
        "SATA".into()
    } else {
        "Unknown".into()
    }
}

/// First mountpoint and fs type from /proc/mounts (filters pseudo-fs types).
fn find_mount(proc_dir: &Path, dev: &str) -> (Option<String>, Option<String>) {
    if let Ok(text) = std::fs::read_to_string(proc_dir.join("mounts"))
        .or_else(|_| std::fs::read_to_string(proc_dir.join("self/mounts")))
    {
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let d = fields.next().unwrap_or("");
            if d == dev {
                let mp: Option<String> = fields.next().map(String::from);
                let fstype: Option<String> = fields.next().map(String::from);
                return (mp, fstype);
            }
        }
    }
    (None, None)
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
    fn disk_with_partitions_and_temp() {
        let root = std::env::temp_dir().join("nuc-fix-storage");
        let _ = fs::remove_dir_all(&root);
        let disk = root.join("sys/class/block/nvme0n1");
        write(&disk.join("device/model"), "WD_BLACK SN850X\n");
        write(&disk.join("size"), "3907029768");
        write(&disk.join("queue/rotational"), "0");
        write(&disk.join("device/firmware_rev"), "620381WD");
        let part = root.join("sys/class/block/nvme0n1/nvme0n1p1");
        write(&part.join("size"), "489216000");
        write(&part.join("partition"), "1");
        let hw = root.join("sys/class/hwmon/hwmon2");
        write(&hw.join("name"), "nvme");
        write(&hw.join("temp1_input"), "41850");
        write(&hw.join("temp1_crit"), "93850");
        std::os::unix::fs::symlink(disk.join("device"), hw.join("device")).unwrap();
        let m = StorageMonitor::new(SourcePaths::fixture(&root));
        let disks = m.discover();
        assert_eq!(disks.len(), 1);
        let d = &disks[0];
        assert_eq!(d.model.as_deref(), Some("WD_BLACK SN850X"));
        assert_eq!(d.transport, "NVMe");
        assert!(!d.rotational);
        assert_eq!(d.temp_c, Some(41.85));
        assert_eq!(d.temp_crit_c, Some(93.85));
        assert_eq!(d.firmware.as_deref(), Some("620381WD"));
        assert_eq!(d.partitions.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn virtual_devices_skipped() {
        let root = std::env::temp_dir().join("nuc-fix-storage-virt");
        let _ = fs::remove_dir_all(&root);
        write(&root.join("sys/class/block/zram0/size"), "0");
        write(&root.join("sys/class/block/ram0/size"), "0");
        let m = StorageMonitor::new(SourcePaths::fixture(&root));
        assert!(m.discover().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prefix_named_disks_and_nvme_temperatures_are_not_confused() {
        let root = std::env::temp_dir().join("nuc-storage-paired");
        let _ = fs::remove_dir_all(&root);
        for (disk, hwmon, temperature) in [
            ("nvme0n1", "hwmon0", "40000"),
            ("nvme0n10", "hwmon1", "90000"),
        ] {
            let path = root.join("sys/class/block").join(disk);
            write(&path.join("size"), "1000");
            write(&path.join("device/model"), disk);
            let hw = root.join("sys/class/hwmon").join(hwmon);
            write(&hw.join("name"), "nvme");
            write(&hw.join("temp1_input"), temperature);
            std::os::unix::fs::symlink(path.join("device"), hw.join("device")).unwrap();
        }
        let disks = StorageMonitor::new(SourcePaths::fixture(&root)).discover();
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].temp_c, Some(40.0));
        assert_eq!(disks[1].temp_c, Some(90.0));
        fs::remove_dir_all(root).unwrap();
    }
}
