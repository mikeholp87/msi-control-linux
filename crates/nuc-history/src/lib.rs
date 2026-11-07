//! SQLite-backed sampling history with minute aggregation and retention.
//!
//! Design rules:
//! - every numeric column is stored exactly as observed; a missing reading is
//!   a NULL, never zero and never interpolated;
//! - raw samples are kept `raw_retention_h` hours, 1-minute aggregates for
//!   `minute_agg_retention_days` days;
//! - aggregation is incremental: a `meta` key tracks the highest aggregated
//!   raw timestamp so each minute is computed once;
//! - the store is synchronous (`Mutex<Connection>`); the GUI flushes its
//!   buffer on the record heartbeat and queries on idle.

use nuc_core::{AppError, MutexExt, Snapshot};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

pub const SCHEMA_VERSION: i64 = 1;

/// The SQLite file lives under the XDG data dir by default.
pub const DEFAULT_DB_NAME: &str = "history.db";

/// One raw recorded sample (already reduced from a Snapshot; NULL = absent).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SampleRow {
    pub ts_ms: i64,
    /// false when a sampling gap (suspend etc.) invalidated delta metrics.
    pub deltas_valid: bool,
    pub cpu_util: Option<f64>,
    pub pkg_power_w: Option<f64>,
    pub cpu_temp_c: Option<f64>,
    pub nvme_temp_c: Option<f64>,
    pub fan_rpm: Option<f64>,
    pub fan_fault: Option<i64>,
    pub mem_used_pct: Option<f64>,
    pub mem_pressure: Option<f64>,
    pub gpu_util: Option<f64>,
    pub gpu_freq_mhz: Option<i64>,
    pub profile: Option<String>,
}

/// One 1-minute aggregate row.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MinuteRow {
    pub ts_ms: i64,
    pub coverage: i64,
    pub delta_samples: i64, // samples with valid deltas (power/util included)
    pub cpu_util_avg: Option<f64>,
    pub cpu_util_max: Option<f64>,
    pub pkg_power_avg_w: Option<f64>,
    pub pkg_power_max_w: Option<f64>,
    pub cpu_temp_avg_c: Option<f64>,
    pub cpu_temp_max_c: Option<f64>,
    pub nvme_temp_max_c: Option<f64>,
    pub fan_rpm_avg: Option<f64>,
    pub mem_used_max_pct: Option<f64>,
    pub gpu_util_avg: Option<f64>,
    pub gpu_util_max: Option<f64>,
}

/// SQLite-backed history store. All writes are buffered in Rust and flushed
/// in a transaction to keep the GUI loop cheap.
pub struct HistoryStore {
    conn: Mutex<Connection>,
    buf: Mutex<Vec<SampleRow>>,
}

/// Telemetry history is per-user: directory 0700, database 0600 (SQLite
/// creates the WAL/SHM sidecars with the database's mode).
fn private_perms(dir: &Path, db: &Path) -> nuc_core::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let io = |e: std::io::Error| AppError::Other(format!("history permissions: {e}"));
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(db)
        .map_err(io)?;
    std::fs::set_permissions(db, std::fs::Permissions::from_mode(0o600)).map_err(io)
}

impl HistoryStore {
    /// Open (creating if needed) a store at `dir/history.db`, applying schema
    /// migrations and WAL mode.
    pub fn open_in_dir(dir: &Path) -> nuc_core::Result<HistoryStore> {
        std::fs::create_dir_all(dir).map_err(|e| AppError::Other(format!("history dir: {e}")))?;
        let db = dir.join(DEFAULT_DB_NAME);
        private_perms(dir, &db)?;
        let conn = Connection::open(&db)
            .map_err(|e| AppError::Other(format!("open {}: {e}", db.display())))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| AppError::Other(format!("busy_timeout: {e}")))?;
        init_schema(&conn)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| AppError::Other(format!("WAL: {e}")))?;
        Ok(HistoryStore {
            conn: Mutex::new(conn),
            buf: Mutex::new(Vec::new()),
        })
    }

    /// Default location: XDG data dir (`~/.local/share/nuc-control`).
    pub fn open_default() -> nuc_core::Result<HistoryStore> {
        Self::open_in_dir(&nuc_core::config::data_dir())
    }

    /// Session-only fallback; never use a shared, predictable temporary path.
    pub fn open_in_memory() -> nuc_core::Result<HistoryStore> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            buf: Mutex::new(Vec::new()),
        })
    }

    /// Inspect existing history without creating directories or changing schema.
    pub fn open_read_only_in_dir(dir: &Path) -> nuc_core::Result<HistoryStore> {
        let conn = Connection::open_with_flags(
            dir.join(DEFAULT_DB_NAME),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(db_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
            buf: Mutex::new(Vec::new()),
        })
    }

    /// Reduce a monitoring Snapshot to a row and buffer it. Nothing touches
    /// the DB until `flush()` — cheap for the sampler loop.
    pub fn record(&self, snap: &Snapshot) {
        let row = row_from(snap);
        self.buf.lock_or_recover().push(row);
    }

    /// Number of rows waiting for `flush()`.
    pub fn buffered(&self) -> usize {
        self.buf.lock_or_recover().len()
    }

    /// Write buffered rows in a single transaction. Returns rows written.
    pub fn flush(&self) -> nuc_core::Result<usize> {
        let mut buf = self.buf.lock_or_recover();
        if buf.is_empty() {
            return Ok(0);
        }
        let rows = buf.len();
        let mut conn = self.conn.lock_or_recover();
        {
            let tx = conn
                .transaction()
                .map_err(|e| AppError::Other(format!("db tx: {e}")))?;
            {
                let mut ins = tx
                    .prepare_cached(
                        "INSERT OR REPLACE INTO samples (
                            ts_ms, deltas_valid, cpu_util, pkg_power_w, cpu_temp_c,
                            nvme_temp_c, fan_rpm, fan_fault, mem_used_pct, mem_pressure,
                            gpu_util, gpu_freq_mhz, profile
                        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    )
                    .map_err(db_err)?;
                for r in buf.iter() {
                    ins.execute(rusqlite::params![
                        r.ts_ms,
                        r.deltas_valid as i64,
                        r.cpu_util,
                        r.pkg_power_w,
                        r.cpu_temp_c,
                        r.nvme_temp_c,
                        r.fan_rpm,
                        r.fan_fault,
                        r.mem_used_pct,
                        r.mem_pressure,
                        r.gpu_util,
                        r.gpu_freq_mhz,
                        r.profile,
                    ])
                    .map_err(db_err)?;
                }
            }
            tx.commit()
                .map_err(|e| AppError::Other(format!("db commit: {e}")))?;
        }
        buf.clear();
        Ok(rows)
    }

    /// Aggregate any fully-covered raw minutes newer than the last processed
    /// one into the `minutes` table. Returns number of minutes written.
    pub fn aggregate_minutes(&self) -> nuc_core::Result<usize> {
        let mut conn = self.conn.lock_or_recover();
        let up_to: i64 = get_meta(&conn, "agg_up_to_ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // A minute is finished only when we hold a raw sample in a LATER
        // minute (future samples could still land inside an open minute).
        let newest_raw: i64 = conn
            .query_row("SELECT MAX(ts_ms) FROM samples", [], |r| r.get(0))
            .unwrap_or(0);
        let finished_through = minute_floor(newest_raw); // exclusive upper bound
        if finished_through <= up_to {
            return Ok(0);
        }
        let mut done = 0usize;
        {
            let tx = conn
                .transaction()
                .map_err(|e| AppError::Other(format!("db tx: {e}")))?;
            {
                let mut agg = tx
                    .prepare_cached(
                        "SELECT
                            ts_ms/60000*60000 AS minute,
                            COUNT(*),
                            SUM(cpu_util IS NOT NULL),
                            AVG(cpu_util),
                            MAX(cpu_util),
                            AVG(CASE WHEN deltas_valid THEN pkg_power_w END),
                            MAX(CASE WHEN deltas_valid THEN pkg_power_w END),
                            AVG(cpu_temp_c),
                            MAX(cpu_temp_c),
                            MAX(nvme_temp_c),
                            AVG(fan_rpm),
                            MAX(mem_used_pct),
                            AVG(CASE WHEN deltas_valid THEN gpu_util END),
                            MAX(CASE WHEN deltas_valid THEN gpu_util END),
                            SUM(deltas_valid)
                         FROM samples
                         WHERE ts_ms >= ?1 AND ts_ms < ?2
                         GROUP BY minute
                         ORDER BY minute",
                    )
                    .map_err(db_err)?;
                let mut rows = agg
                    .query(rusqlite::params![up_to, finished_through])
                    .map_err(db_err)?;
                let mut ins = tx
                    .prepare_cached(
                        "INSERT OR REPLACE INTO minutes (
                            ts_ms, coverage, delta_samples, cpu_util_avg, cpu_util_max,
                            pkg_power_avg_w, pkg_power_max_w, cpu_temp_avg_c, cpu_temp_max_c,
                            nvme_temp_max_c, fan_rpm_avg, mem_used_max_pct,
                            gpu_util_avg, gpu_util_max
                        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    )
                    .map_err(db_err)?;
                while let Some(r) = rows.next().map_err(db_err)? {
                    ins.execute(rusqlite::params![
                        row_i64(r, 0)?,
                        row_i64(r, 1)?,
                        row_i64(r, 14)?,
                        row_f64(r, 3)?,
                        row_f64(r, 4)?,
                        row_f64(r, 5)?,
                        row_f64(r, 6)?,
                        row_f64(r, 7)?,
                        row_f64(r, 8)?,
                        row_f64(r, 9)?,
                        row_f64(r, 10)?,
                        row_f64(r, 11)?,
                        row_f64(r, 12)?,
                        row_f64(r, 13)?,
                    ])
                    .map_err(db_err)?;
                    done += 1;
                }
            }
            set_meta(&tx, "agg_up_to_ms", &finished_through.to_string())?;
            tx.commit()
                .map_err(|e| AppError::Other(format!("db commit: {e}")))?;
        }
        Ok(done)
    }

    /// Append an event row (alerts, profile switches, diagnostics…).
    pub fn record_event(&self, ts_ms: i64, kind: &str, message: &str) -> nuc_core::Result<()> {
        let conn = self.conn.lock_or_recover();
        conn.execute(
            "INSERT INTO events (ts_ms, kind, message) VALUES (?1, ?2, ?3)",
            rusqlite::params![ts_ms, kind, message],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Raw samples in [from_ms, to_ms], ordered by time.
    pub fn query_samples(&self, from_ms: i64, to_ms: i64) -> nuc_core::Result<Vec<SampleRow>> {
        let conn = self.conn.lock_or_recover();
        let mut stmt = conn
            .prepare(
                "SELECT ts_ms, deltas_valid, cpu_util, pkg_power_w, cpu_temp_c, nvme_temp_c, \
                      fan_rpm, fan_fault, mem_used_pct, mem_pressure, gpu_util, gpu_freq_mhz, \
                      profile FROM samples WHERE ts_ms >= ?1 AND ts_ms <= ?2 ORDER BY ts_ms",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(rusqlite::params![from_ms, to_ms])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(db_err)? {
            out.push(SampleRow {
                ts_ms: row_i64(r, 0)?,
                deltas_valid: row_i64(r, 1)? != 0,
                cpu_util: row_f64(r, 2)?,
                pkg_power_w: row_f64(r, 3)?,
                cpu_temp_c: row_f64(r, 4)?,
                nvme_temp_c: row_f64(r, 5)?,
                fan_rpm: row_f64(r, 6)?,
                fan_fault: row_i64_opt(r, 7)?,
                mem_used_pct: row_f64(r, 8)?,
                mem_pressure: row_f64(r, 9)?,
                gpu_util: row_f64(r, 10)?,
                gpu_freq_mhz: row_i64_opt(r, 11)?,
                profile: r.get(12).map_err(db_err)?,
            });
        }
        Ok(out)
    }

    /// Minute aggregates in [from_ms, to_ms], ordered by time.
    pub fn query_minutes(&self, from_ms: i64, to_ms: i64) -> nuc_core::Result<Vec<MinuteRow>> {
        let conn = self.conn.lock_or_recover();
        let mut stmt = conn
            .prepare(
                "SELECT ts_ms, coverage, delta_samples, cpu_util_avg, cpu_util_max, \
                      pkg_power_avg_w, pkg_power_max_w, cpu_temp_avg_c, cpu_temp_max_c, \
                      nvme_temp_max_c, fan_rpm_avg, mem_used_max_pct, gpu_util_avg, \
                      gpu_util_max FROM minutes WHERE ts_ms >= ?1 AND ts_ms <= ?2 ORDER BY ts_ms",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(rusqlite::params![from_ms, to_ms])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(db_err)? {
            out.push(MinuteRow {
                ts_ms: row_i64(r, 0)?,
                coverage: row_i64(r, 1)?,
                delta_samples: row_i64(r, 2)?,
                cpu_util_avg: row_f64(r, 3)?,
                cpu_util_max: row_f64(r, 4)?,
                pkg_power_avg_w: row_f64(r, 5)?,
                pkg_power_max_w: row_f64(r, 6)?,
                cpu_temp_avg_c: row_f64(r, 7)?,
                cpu_temp_max_c: row_f64(r, 8)?,
                nvme_temp_max_c: row_f64(r, 9)?,
                fan_rpm_avg: row_f64(r, 10)?,
                mem_used_max_pct: row_f64(r, 11)?,
                gpu_util_avg: row_f64(r, 12)?,
                gpu_util_max: row_f64(r, 13)?,
            });
        }
        Ok(out)
    }

    /// Events in [from_ms, to_ms] (ts, kind, message).
    pub fn query_events(&self, from_ms: i64, to_ms: i64) -> nuc_core::Result<Vec<EventRow>> {
        let conn = self.conn.lock_or_recover();
        let mut stmt = conn
            .prepare(
                "SELECT ts_ms, kind, message FROM events WHERE ts_ms >= ?1 AND ts_ms <= ?2 \
                      ORDER BY ts_ms",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(rusqlite::params![from_ms, to_ms])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(db_err)? {
            out.push(EventRow {
                ts_ms: r.get(0).map_err(db_err)?,
                kind: r.get(1).map_err(db_err)?,
                message: r.get(2).map_err(db_err)?,
            });
        }
        Ok(out)
    }

    /// Expire old data, then prune oldest raw rows, aggregates and events to
    /// fit the MiB budget. Returns (raw_deleted, min_deleted).
    pub fn apply_retention(
        &self,
        raw_hours: u32,
        minutes_days: u32,
        max_mib: u32,
    ) -> nuc_core::Result<(usize, usize)> {
        let conn = self.conn.lock_or_recover();
        let now = now_ms();
        let raw_cutoff = now - (raw_hours as i64) * 3_600_000;
        let min_cutoff = now - (minutes_days as i64) * 86_400_000;
        let raw_del = conn
            .execute(
                "DELETE FROM samples WHERE ts_ms < ?1",
                rusqlite::params![raw_cutoff],
            )
            .map_err(db_err)?;
        let mut min_del = conn
            .execute(
                "DELETE FROM minutes WHERE ts_ms < ?1",
                rusqlite::params![min_cutoff],
            )
            .map_err(db_err)?;
        conn.execute(
            "DELETE FROM events WHERE ts_ms < ?1",
            rusqlite::params![min_cutoff],
        )
        .map_err(db_err)?;
        // Free pages are reusable; counting them as live data deletes too much.
        let mut extra = 0usize;
        let limit = (max_mib as i64) * 1024 * 1024;
        if limit > 0 {
            loop {
                let pages: i64 = conn
                    .query_row("PRAGMA page_count", [], |r| r.get(0))
                    .map_err(db_err)?;
                let free: i64 = conn
                    .query_row("PRAGMA freelist_count", [], |r| r.get(0))
                    .map_err(db_err)?;
                let page_size: i64 = conn
                    .query_row("PRAGMA page_size", [], |r| r.get(0))
                    .map_err(db_err)?;
                if (pages - free) * page_size <= limit {
                    break;
                }
                // Cut the oldest 5000 raw rows per pass (fast, incremental).
                let n = conn.execute(
                    "DELETE FROM samples WHERE ts_ms IN (SELECT ts_ms FROM samples ORDER BY ts_ms LIMIT 500)", []
                ).map_err(db_err)?;
                extra += n;
                if n == 0 {
                    let n = conn.execute("DELETE FROM minutes WHERE ts_ms IN (SELECT ts_ms FROM minutes ORDER BY ts_ms LIMIT 500)", []).map_err(db_err)?;
                    min_del += n;
                    if n == 0 && conn.execute("DELETE FROM events WHERE rowid IN (SELECT rowid FROM events ORDER BY ts_ms LIMIT 500)", []).map_err(db_err)? == 0 {
                        break;
                    }
                }
            }
            let allocated: i64 = conn
                .query_row(
                    "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                    [],
                    |r| r.get(0),
                )
                .map_err(db_err)?;
            if allocated > limit {
                conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
                    .map_err(db_err)?;
            }
        }
        Ok((raw_del + extra, min_del))
    }

    /// Size on disk (main DB file, bytes) for the settings UI.
    pub fn size_bytes(&self) -> nuc_core::Result<u64> {
        let conn = self.conn.lock_or_recover();
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .map_err(db_err)?;
        let ps: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .map_err(db_err)?;
        Ok(u64::try_from((pages * ps).max(0)).unwrap_or(0))
    }

    /// Row counts for the diagnostics page.
    pub fn row_counts(&self) -> nuc_core::Result<(i64, i64, i64)> {
        let conn = self.conn.lock_or_recover();
        let s: i64 = conn
            .query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))
            .map_err(db_err)?;
        let m: i64 = conn
            .query_row("SELECT COUNT(*) FROM minutes", [], |r| r.get(0))
            .map_err(db_err)?;
        let e: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .map_err(db_err)?;
        Ok((s, m, e))
    }
}

impl Drop for HistoryStore {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            eprintln!("history flush on close failed: {error}");
        }
    }
}

// ------------------------------------------------------------ SNAPSHOT REDUCTION

/// Reduce a full Snapshot to the stored row. Sensors are matched by source;
/// "unavailable" readings become NULL columns.
fn row_from(s: &Snapshot) -> SampleRow {
    let cpu_temp = s
        .temperatures
        .iter()
        .filter(|t| t.source.contains("coretemp") || t.id.contains("coretemp"))
        .filter_map(|t| t.value_c)
        .fold(None::<f64>, |acc, v| {
            acc.map_or(Some(v), |a| Some(a.max(v)))
        });
    let nvme_temp = s
        .temperatures
        .iter()
        .find(|t| t.source.contains("nvme") || t.id.contains("nvme"))
        .and_then(|t| t.value_c);
    let fan = s.fans.first();
    let mem_pct = match (s.memory.used_kib, s.memory.total_kib) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 * 100.0 / t as f64),
        _ => None,
    };
    SampleRow {
        ts_ms: s.ts_ms,
        deltas_valid: s.deltas_valid,
        cpu_util: s.cpu.usage_total,
        pkg_power_w: s.cpu.interval_power_w,
        cpu_temp_c: cpu_temp,
        nvme_temp_c: nvme_temp,
        fan_rpm: fan.filter(|f| f.source != "acpi_fan").and_then(|f| f.rpm),
        fan_fault: fan.and_then(|f| f.fault).map(|b| b as i64),
        mem_used_pct: mem_pct,
        mem_pressure: s.memory.pressure,
        gpu_util: s.gpu.as_ref().and_then(|g| g.utilization_pct),
        gpu_freq_mhz: s.gpu.as_ref().and_then(|g| g.freq_mhz).map(|v| v as i64),
        profile: s.profile.as_ref().and_then(|p| p.active.clone()),
    }
}

/// Floor a timestamp to its minute boundary (UTC).
fn minute_floor(ts_ms: i64) -> i64 {
    ts_ms.div_euclid(60_000) * 60_000
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(nuc_core::millis_i64)
        .unwrap_or(0)
}

fn init_schema(conn: &Connection) -> nuc_core::Result<()> {
    let has_meta: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    if has_meta && let Some(version) = get_meta(conn, "schema_version") {
        let version: i64 = version
            .parse()
            .map_err(|_| AppError::Other("invalid history schema version".into()))?;
        if version > SCHEMA_VERSION {
            return Err(AppError::Unsupported(format!(
                "history schema {version} is newer than supported version {SCHEMA_VERSION}"
            )));
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS samples (
            ts_ms INTEGER PRIMARY KEY,
            deltas_valid INTEGER NOT NULL DEFAULT 1,
            cpu_util REAL, pkg_power_w REAL, cpu_temp_c REAL, nvme_temp_c REAL,
            fan_rpm REAL, fan_fault INTEGER, mem_used_pct REAL, mem_pressure REAL,
            gpu_util REAL, gpu_freq_mhz INTEGER, profile TEXT
        );
        CREATE TABLE IF NOT EXISTS minutes (
            ts_ms INTEGER PRIMARY KEY,
            coverage INTEGER NOT NULL, delta_samples INTEGER NOT NULL,
            cpu_util_avg REAL, cpu_util_max REAL,
            pkg_power_avg_w REAL, pkg_power_max_w REAL,
            cpu_temp_avg_c REAL, cpu_temp_max_c REAL, nvme_temp_max_c REAL,
            fan_rpm_avg REAL, mem_used_max_pct REAL,
            gpu_util_avg REAL, gpu_util_max REAL
        );
        CREATE TABLE IF NOT EXISTS events (
            ts_ms INTEGER NOT NULL,
            kind TEXT NOT NULL,
            message TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS events_ts ON events (ts_ms);
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )
    .map_err(|e| AppError::Other(format!("schema: {e}")))?;
    set_schema_version(conn)?;
    Ok(())
}

fn set_schema_version(conn: &Connection) -> nuc_core::Result<()> {
    set_meta(conn, "schema_version", &SCHEMA_VERSION.to_string())
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> nuc_core::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )
    .map_err(db_err)?;
    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![key],
        |r| r.get(0),
    )
    .ok()
}

fn row_i64(r: &rusqlite::Row<'_>, idx: usize) -> nuc_core::Result<i64> {
    r.get(idx).map_err(db_err)
}

fn row_f64(r: &rusqlite::Row<'_>, idx: usize) -> nuc_core::Result<Option<f64>> {
    r.get(idx).map_err(db_err)
}

fn row_i64_opt(r: &rusqlite::Row<'_>, idx: usize) -> nuc_core::Result<Option<i64>> {
    r.get(idx).map_err(db_err)
}

fn db_err(e: rusqlite::Error) -> AppError {
    if let rusqlite::Error::SqliteFailure(err, _) = &e
        && err.code == rusqlite::ErrorCode::DatabaseLocked
    {
        return AppError::Other("history database locked (another writer active)".into());
    }
    AppError::Other(format!("sqlite: {e}"))
}

/// Row of the `events` table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRow {
    pub ts_ms: i64,
    pub kind: String,
    pub message: String,
}

// ------------------------------------------------------------ TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use nuc_core::{CpuSnapshot, Temperature};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(nuc_core::millis_i64)
            .unwrap()
    }

    fn snap(ts: i64, power: Option<f64>, temp: Option<f64>, util: Option<f64>) -> Snapshot {
        Snapshot {
            ts_ms: ts,
            delta_ms: 100,
            deltas_valid: true,
            peak_power_w: None,
            cpu: CpuSnapshot {
                usage_total: util,
                interval_power_w: power,
                ..Default::default()
            },
            gpu: None,
            temperatures: vec![Temperature {
                id: "coretemp.0/temp1".into(),
                label: "Core 0".into(),
                value_c: temp,
                source: "coretemp".into(),
                ..Default::default()
            }],
            fans: vec![],
            rapl: vec![],
            memory: Default::default(),
            storage: vec![],
            profile: None,
        }
    }

    #[test]
    fn record_flush_query_roundtrip() {
        let dir = std::env::temp_dir().join(format!("nuc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        let t0 = (now() / 60_000) * 60_000; // aligned minute
        store.record(&snap(t0, Some(9.4), Some(40.0), Some(12.5)));
        store.record(&snap(t0 + 1_000, Some(10.1), Some(41.0), None));
        assert_eq!(store.buffered(), 2);
        assert_eq!(store.flush().unwrap(), 2);
        assert_eq!(store.buffered(), 0);
        // flush again writes nothing
        assert_eq!(store.flush().unwrap(), 0);

        let rows = store.query_samples(t0, t0 + 2_000).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cpu_temp_c, Some(40.0));
        assert_eq!(rows[1].cpu_temp_c, Some(41.0));
        // NULL stays NULL (no invented zeros)
        assert_eq!(rows[1].cpu_util, None);
        assert_eq!(rows[0].pkg_power_w, Some(9.4));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minute_aggregation() {
        let dir = std::env::temp_dir().join(format!("nuc-agg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        let t0 = (now() / 60_000) * 60_000; // minute boundary
        // three samples in minute 0, one in minute 1 (incomplete → untouched)
        store.record(&snap(t0, Some(5.0), Some(40.0), Some(10.0)));
        store.record(&snap(t0 + 20_000, Some(7.0), Some(42.0), Some(30.0)));
        store.record(&snap(t0 + 40_000, Some(9.0), Some(44.0), None));
        store.record(&snap(t0 + 70_000, Some(99.0), None, None));
        store.flush().unwrap();

        let n = store.aggregate_minutes().unwrap();
        // minute 0 is finished (a sample exists past it); minute 1 open
        assert_eq!(n, 1);
        let m = store.query_minutes(t0, t0 + 60_000).unwrap();
        assert_eq!(m.len(), 1);
        let m0 = &m[0];
        assert_eq!(m0.ts_ms, t0);
        assert_eq!(m0.coverage, 3);
        assert_eq!(m0.delta_samples, 3);
        assert_eq!(m0.pkg_power_max_w, Some(9.0));
        assert_eq!(m0.cpu_temp_max_c, Some(44.0));
        assert_eq!(m0.gpu_util_max, None); // nothing recorded → no zero

        // idempotent: re-aggregating does nothing new
        assert_eq!(store.aggregate_minutes().unwrap(), 0);
        // and the open minute becomes closed by a later sample
        store.record(&snap(t0 + 60_000, Some(105.0), None, None));
        store.record(&snap(t0 + 130_000, Some(1.0), Some(39.0), Some(50.0)));
        store.flush().unwrap();
        assert_eq!(store.aggregate_minutes().unwrap(), 1);
        let all = store.query_minutes(t0, t0 + 200_000).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].coverage, 2);
        assert_eq!(all[1].pkg_power_max_w, Some(105.0));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_roundtrip() {
        let dir = std::env::temp_dir().join(format!("nuc-ev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        let t = now();
        store
            .record_event(t, "alert", "cpu_temp warm (85.0 °C, 3 samples)")
            .unwrap();
        store
            .record_event(t + 1, "profile", "active -> power-saver")
            .unwrap();
        let ev = store.query_events(t - 1, t + 2).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, "alert");
        assert_eq!(ev[1].kind, "profile");
        assert_eq!(ev[1].message, "active -> power-saver");
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_purges_only_oldest() {
        let dir = std::env::temp_dir().join(format!("nuc-ret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        let now = now();
        let recent = now - 600_000; // 10 minutes old
        let ancient = now - 7 * 3_600_000; // 7 hours old
        store.record(&snap(recent, None, None, None));
        store.record(&snap(ancient, None, None, None));
        store.flush().unwrap();
        store.aggregate_minutes().unwrap();
        // one minute may have aggregated; raw removal happens regardless
        store.apply_retention(1, 0, 1024).unwrap();
        let remaining = store.query_samples(0, now + 1).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].ts_ms, recent);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_reopen_preserves_data() {
        let dir = std::env::temp_dir().join(format!("nuc-re-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = HistoryStore::open_in_dir(&dir).unwrap();
            store.record(&snap(1_000_000, Some(1.0), None, None));
            // Dropping the store must also persist an unflushed short session.
        }
        {
            let store = HistoryStore::open_in_dir(&dir).unwrap();
            let rows = store.query_samples(0, 2_000_000).unwrap();
            assert_eq!(rows.len(), 1);
            let (s, m, e) = store.row_counts().unwrap();
            assert_eq!((s, m, e), (1, 0, 0));
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_budget_reclaims_space_without_deleting_every_sample() {
        let dir = std::env::temp_dir().join(format!("nuc-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        let t0 = now() - 10_000;
        for i in 0..1800 {
            let mut s = snap(t0 + i, None, None, None);
            s.profile = Some(nuc_core::PowerProfileState {
                backend: String::new(),
                active: Some("x".repeat(1024)),
                profiles: vec![],
                degraded: false,
                degraded_reason: None,
            });
            store.record(&s);
        }
        store.flush().unwrap();
        assert!(store.size_bytes().unwrap() > 1024 * 1024);
        store.apply_retention(24, 30, 1).unwrap();
        let rows = store.query_samples(0, now()).unwrap();
        assert!(!rows.is_empty() && rows.len() < 1800);
        assert_eq!(rows.last().unwrap().ts_ms, t0 + 1799);
        assert!(store.size_bytes().unwrap() <= 1024 * 1024);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn size_budget_also_bounds_events() {
        let store = HistoryStore::open_in_memory().unwrap();
        for i in 0..1500 {
            store
                .record_event(now() + i, "test", &"x".repeat(1024))
                .unwrap();
        }
        store.apply_retention(24, 30, 1).unwrap();
        assert!(store.size_bytes().unwrap() <= 1024 * 1024);
    }

    #[test]
    fn newer_schema_is_rejected_and_preserved() {
        let dir = std::env::temp_dir().join(format!("nuc-schema-future-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        set_meta(&store.conn.lock().unwrap(), "schema_version", "99").unwrap();
        drop(store);
        assert!(HistoryStore::open_in_dir(&dir).is_err());
        let store = HistoryStore::open_read_only_in_dir(&dir).unwrap();
        assert_eq!(
            get_meta(&store.conn.lock().unwrap(), "schema_version").as_deref(),
            Some("99")
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uncertain_fan_values_are_not_stored_as_measured_rpm() {
        let s = Snapshot {
            fans: vec![nuc_core::FanReading {
                rpm: Some(61.0),
                source: "acpi_fan".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(row_from(&s).fan_rpm, None);
    }

    #[test]
    fn history_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nuc-hist-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        store.flush().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(DEFAULT_DB_NAME)), 0o600);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
