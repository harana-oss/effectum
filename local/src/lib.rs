pub mod add_job;
mod error;
pub mod job_status;
mod migrations;
mod shared_state;
mod worker_list;

pub mod job;
pub mod job_registry;
mod sqlite_functions;
#[cfg(test)]
mod test_util;
pub mod worker;

use std::{path::Path, sync::Arc};

pub use error::{Error, Result};
use rusqlite::Connection;
use shared_state::{SharedState, SharedStateData};
use sqlite_functions::register_functions;
use time::Duration;
use worker_list::Workers;

pub(crate) type SmartString = smartstring::SmartString<smartstring::LazyCompact>;

pub struct Retries {
    pub max_retries: u32,
    pub backoff_multiplier: f32,
    pub backoff_randomization: f32,
    pub backoff_initial_interval: Duration,
}

impl Default for Retries {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_multiplier: 2f32,
            backoff_randomization: 0.2,
            backoff_initial_interval: Duration::seconds(20),
        }
    }
}

pub struct NewJob {
    job_type: String,
    priority: Option<i64>,
    /// When to run the job. `None` means to run it right away.
    run_at: Option<time::OffsetDateTime>,
    payload: Vec<u8>,
    retries: Retries,
    timeout: time::Duration,
    heartbeat_increment: time::Duration,
}

struct Tasks {
    close: tokio::sync::watch::Sender<()>,
    worker_count_rx: tokio::sync::watch::Receiver<usize>,
}

pub struct Queue {
    state: SharedState,
    tasks: Option<Tasks>,
}

impl Queue {
    /// Open or create a new Queue database at the given path.
    ///
    /// Note that if you use an existing database file, this queue will set the journal style to
    /// WAL mode.
    pub fn new(file: &Path) -> Result<Queue> {
        let mut conn = Connection::open(file).map_err(Error::OpenDatabase)?;
        conn.pragma_update(None, "journal", "wal")
            .map_err(Error::OpenDatabase)?;

        register_functions(&mut conn)?;
        crate::migrations::migrate(&mut conn)?;

        let (close_tx, close_rx) = tokio::sync::watch::channel(());

        let pool_cfg = deadpool_sqlite::Config::new(file);
        let read_conn_pool = pool_cfg.create_pool(deadpool_sqlite::Runtime::Tokio1)?;

        let (worker_count_tx, worker_count_rx) = tokio::sync::watch::channel(0);
        let shared_state = SharedState(Arc::new(SharedStateData {
            db: std::sync::Mutex::new(conn),
            read_conn_pool,
            workers: tokio::sync::RwLock::new(Workers::new(worker_count_tx)),
            close: close_rx,
        }));

        // TODO Optionally clean up running jobs here, treating them all as failures and scheduling
        // for retry. For later server mode, we probably want to do something more intelligent so
        // that we can continue to receive "job finished" notifications. This will probably involve
        // persisting the worker information to the database so we can properly recover it.

        // TODO task to monitor expired jobs
        // TODO task to schedule recurring jobs
        // TODO Optional task to delete old jobs from `done_jobs`

        let q = Queue {
            state: shared_state,
            tasks: Some(Tasks {
                close: close_tx,
                worker_count_rx,
            }),
        };

        Ok(q)
    }

    async fn wait_for_workers_to_stop(
        tasks: &mut Tasks,
        timeout: std::time::Duration,
    ) -> Result<()> {
        if *tasks.worker_count_rx.borrow_and_update() == 0 {
            return Ok(());
        }

        let timeout = tokio::time::sleep(timeout);
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                _ = &mut timeout => return Err(Error::Timeout),
                res = tasks.worker_count_rx.changed() => {
                    if res.is_err() || *tasks.worker_count_rx.borrow() == 0 {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Stop the queue, and wait for existing workers to finish.
    pub async fn close(&mut self, timeout: std::time::Duration) -> Result<()> {
        if let Some(mut tasks) = self.tasks.take() {
            tasks.close.send(()).ok();
            Self::wait_for_workers_to_stop(&mut tasks, timeout).await?;
        }
        Ok(())
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        if let Some(tasks) = self.tasks.take() {
            tasks.close.send(()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_util::create_test_queue;

    #[tokio::test]
    async fn create_queue() {
        create_test_queue();
    }
}
