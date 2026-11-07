use nuc_core::{Snapshot, config::History};
use nuc_history::HistoryStore;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

enum Command {
    Record(Box<Snapshot>),
    Maintain(History, bool),
    Stop,
}

pub struct HistoryWorker {
    tx: SyncSender<Command>,
    updates: Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl HistoryWorker {
    pub fn start(store: HistoryStore, persistent: bool) -> Self {
        let (tx, rx) = mpsc::sync_channel(256);
        let (updates_tx, updates) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut error = None;
            while let Ok(command) = rx.recv() {
                match command {
                    Command::Record(snap) if error.is_none() => store.record(&snap),
                    Command::Record(_) => {}
                    Command::Maintain(cfg, retention) => {
                        let result = store
                            .flush()
                            .and_then(|_| store.aggregate_minutes())
                            .and_then(|_| {
                                if retention {
                                    store.apply_retention(
                                        cfg.raw_retention_h,
                                        cfg.minute_agg_retention_days,
                                        cfg.max_db_mib,
                                    )?;
                                }
                                store.row_counts()
                            });
                        error = result.as_ref().err().map(ToString::to_string);
                        let status = match result {
                            Ok((s, m, e)) => format!(
                                "{s} samples · {m} minute aggregates · {e} events recorded{}",
                                if persistent {
                                    ""
                                } else {
                                    " (session-only; not saved to disk)"
                                }
                            ),
                            Err(err) => format!("History recording paused; retrying: {err}"),
                        };
                        let _ = updates_tx.send(status);
                    }
                    Command::Stop => break,
                }
            }
            if let Err(err) = store.flush() {
                eprintln!("history flush on close failed: {err}");
            }
        });
        Self {
            tx,
            updates,
            thread: Some(thread),
        }
    }

    pub fn record(&self, snap: Snapshot) -> Result<(), &'static str> {
        self.tx
            .try_send(Command::Record(Box::new(snap)))
            .map_err(|err| match err {
                TrySendError::Full(_) => "History queue is full; this sample was not recorded",
                TrySendError::Disconnected(_) => "History worker stopped; sample was not recorded",
            })
    }

    pub fn maintain(&self, cfg: History, retention: bool) {
        let _ = self.tx.try_send(Command::Maintain(cfg, retention));
    }

    pub fn latest_status(&self) -> Option<String> {
        self.updates.try_iter().last()
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_flushes_queued_samples() {
        let dir =
            std::env::temp_dir().join(format!("nuc-gui-history-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        let mut worker = HistoryWorker::start(store, true);
        worker
            .record(Snapshot {
                ts_ms: 12345,
                ..Default::default()
            })
            .unwrap();
        worker.shutdown();
        let store = HistoryStore::open_in_dir(&dir).unwrap();
        assert_eq!(store.query_samples(0, 20000).unwrap().len(), 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
