#![warn(missing_docs)]
//! A SQLite-based task queue library that allows running background jobs without requiring
//! external dependencies.

mod add_job;
mod error;
mod job_status;
mod migrations;
mod shared_state;
mod worker_list;

mod db_writer;
mod job;
mod job_registry;
mod pending_jobs;
mod sqlite_functions;
#[cfg(test)]
mod test_util;
mod worker;

use std::{path::Path, sync::Arc, time::Duration};

use db_writer::{db_writer_worker, DbOperation, DbOperationType};
use deadpool_sqlite::{Hook, HookError, HookErrorCause};
use pending_jobs::monitor_pending_jobs;
use rusqlite::Connection;
use shared_state::{SharedState, SharedStateData};
use sqlite_functions::register_functions;
use tokio::task::JoinHandle;
use worker::log_error;
use worker_list::Workers;

pub use error::{Error, Result};
pub use job::{Job, JobData};
pub use job_registry::{JobDef, JobDefBuilder, JobRegistry};
pub use job_status::{JobState, JobStatus, RunInfo};
pub use worker::{Worker, WorkerBuilder};

pub(crate) type SmartString = smartstring::SmartString<smartstring::LazyCompact>;

/// `Retries` controls the exponential backoff behavior when retrying failed jobs.
#[derive(Debug, Clone)]
pub struct Retries {
    /// How many times to retry a job before it is considered to have failed permanently.
    pub max_retries: u32,
    /// How long to wait before retrying the first time. Defaults to 20 seconds.
    pub backoff_initial_interval: Duration,
    /// For each retry after the first, the backoff time will be multiplied by `backoff_multiplier ^ current_retry`.
    /// Defaults to `2`, which will double the backoff time for each retry.
    pub backoff_multiplier: f32,
    /// To avoid pathological cases where multiple jobs are retrying simultaneously, a
    /// random percentage will be added to the backoff time when a job is rescheduled.
    /// `backoff_randomization` is the maximum percentage to add.
    pub backoff_randomization: f32,
}

impl Default for Retries {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_multiplier: 2f32,
            backoff_randomization: 0.2,
            backoff_initial_interval: Duration::from_secs(20),
        }
    }
}

/// A job to be submitted to the queue.
#[derive(Debug, Clone)]
pub struct NewJob {
    /// The name of the job, which matches the name used in the [JobDef] for the job.
    pub job_type: String,
    /// Jobs with higher `priority` will be executed first.
    pub priority: i32,
    /// Jobs that are expected to take more processing resources can be given a higher weight
    /// to account for this. A worker counts the job's weight (1, by default) against its
    /// maximum concurrency when deciding how many jobs it can execute. For example,
    /// a worker with a `max_concurrency` of 10 would run three jobs at a time if each
    /// had a weight of three.
    ///
    /// For example, a video transcoding task might alter the weight depending on the resolution of
    /// the video or the processing requirements of the codec for each run.
    pub weight: u32,
    /// When to run the job. `None` means to run it right away.
    pub run_at: Option<time::OffsetDateTime>,
    /// The payload to pass to the job when it runs.
    pub payload: Vec<u8>,
    /// Retry behavior when the job fails.
    pub retries: Retries,
    /// How long to allow the job to run before it is considered failed.
    pub timeout: Duration,
    /// How much extra time a heartbeat will add to the expiration time.
    pub heartbeat_increment: Duration,
}

impl Default for NewJob {
    fn default() -> Self {
        Self {
            job_type: Default::default(),
            priority: 0,
            weight: 1,
            run_at: Default::default(),
            payload: Default::default(),
            retries: Default::default(),
            timeout: Duration::from_secs(300),
            heartbeat_increment: Duration::from_secs(120),
        }
    }
}

struct Tasks {
    close: tokio::sync::watch::Sender<()>,
    worker_count_rx: tokio::sync::watch::Receiver<usize>,
    _pending_jobs_monitor: JoinHandle<()>,
    db_write_worker: std::thread::JoinHandle<()>,
}

/// The queue itself, which consists of the SQLite connection and tasks to monitor running jobs.
pub struct Queue {
    state: SharedState,
    tasks: std::sync::Mutex<Option<Tasks>>,
}

impl Queue {
    /// Open or create a new Queue database at the given path.
    ///
    /// Note that if you use an existing database file, this queue will set the journal style to
    /// WAL mode.
    pub async fn new(file: &Path) -> Result<Queue> {
        let mut conn = Connection::open(file).map_err(Error::open_database)?;
        conn.pragma_update(None, "journal", "wal")
            .map_err(Error::open_database)?;
        conn.pragma_update(None, "synchronous", "normal")
            .map_err(Error::open_database)?;

        register_functions(&mut conn)?;
        crate::migrations::migrate(&mut conn)?;

        let (close_tx, close_rx) = tokio::sync::watch::channel(());

        let read_conn_pool = deadpool_sqlite::Config::new(file)
            .builder(deadpool_sqlite::Runtime::Tokio1)
            .map_err(Error::open_database)?
            .recycle_timeout(Some(Duration::from_secs(5 * 60)))
            .post_create(Hook::async_fn(move |conn, _| {
                Box::pin(async move {
                    conn.interact(register_functions)
                        .await
                        .map_err(|e| HookError::Abort(HookErrorCause::Message(e.to_string())))?
                        .map_err(|e| HookError::Abort(HookErrorCause::Backend(e)))?;
                    Ok(())
                })
            }))
            .build()
            .map_err(Error::open_database)?;

        let (worker_count_tx, worker_count_rx) = tokio::sync::watch::channel(0);
        let (pending_jobs_tx, pending_jobs_rx) = tokio::sync::mpsc::channel(10);

        let (db_write_tx, db_write_rx) = tokio::sync::mpsc::channel(50);

        let shared_state = SharedState(Arc::new(SharedStateData {
            read_conn_pool,
            workers: tokio::sync::RwLock::new(Workers::new(worker_count_tx)),
            close: close_rx,
            time: crate::shared_state::Time::new(),
            pending_jobs_tx,
            db_write_tx,
        }));

        let db_write_worker = {
            let shared_state = shared_state.clone();
            std::thread::spawn(move || db_writer_worker(conn, shared_state, db_write_rx))
        };

        let pending_jobs_monitor =
            monitor_pending_jobs(shared_state.clone(), pending_jobs_rx).await?;

        // TODO Optionally clean up running jobs here, treating them all as failures and scheduling
        // for retry. For later server mode, we probably want to do something more intelligent so
        // that we can continue to receive "job finished" notifications. This will probably involve
        // persisting the worker information to the database so we can properly recover it.

        // TODO sweeper task for expired jobs that might not have been caught by the normal mechanism
        // TODO task to schedule recurring jobs
        // TODO Optional task to delete old jobs from `done_jobs`

        let q = Queue {
            state: shared_state,
            tasks: std::sync::Mutex::new(Some(Tasks {
                close: close_tx,
                worker_count_rx,
                _pending_jobs_monitor: pending_jobs_monitor,
                db_write_worker,
            })),
        };

        Ok(q)
    }

    async fn wait_for_workers_to_stop(tasks: &mut Tasks, timeout: Duration) -> Result<()> {
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

    async fn close_internal(mut tasks: Tasks, state: SharedState, timeout: Duration) -> Result<()> {
        tasks.close.send(()).ok();

        let res = Self::wait_for_workers_to_stop(&mut tasks, timeout).await;

        state
            .db_write_tx
            .send(DbOperation {
                worker_id: 0,
                span: tracing::Span::current(),
                operation: DbOperationType::Close,
            })
            .await
            .ok();

        log_error(tokio::task::spawn_blocking(|| tasks.db_write_worker.join()).await);

        res
    }

    /// Stop the queue, and wait for existing workers to finish.
    pub async fn close(&self, timeout: Duration) -> Result<()> {
        let tasks = {
            let mut tasks_holder = self.tasks.lock().unwrap();
            tasks_holder.take()
        };

        if let Some(tasks) = tasks {
            Self::close_internal(tasks, self.state.clone(), timeout).await?;
        }

        Ok(())
    }
}

impl Drop for Queue {
    /// Try to close the queue cleanly as it's dropped.
    fn drop(&mut self) {
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(tasks) = tasks.take() {
            tokio::spawn(Self::close_internal(
                tasks,
                self.state.clone(),
                Duration::from_secs(60),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tracing::{event, Level};

    use crate::{
        job_registry::JobDef,
        job_status::JobState,
        test_util::{
            create_test_queue, job_list, wait_for_job, wait_for_job_status, TestContext,
            TestEnvironment,
        },
        worker::Worker,
        NewJob,
    };

    #[tokio::test]
    async fn create_queue() {
        create_test_queue().await;
    }

    #[tokio::test]
    async fn run_job() {
        let test = TestEnvironment::new().await;

        let _worker = test.worker().build().await.expect("failed to build worker");

        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "counter".to_string(),
                ..Default::default()
            })
            .await
            .expect("failed to add job");

        wait_for_job("job to run", &test.queue, job_id).await;
    }

    #[tokio::test]
    async fn run_multiple_jobs() {
        let test = TestEnvironment::new().await;

        let _worker = test.worker().build().await.expect("failed to build worker");

        let ids = test
            .queue
            .add_jobs(vec![
                NewJob {
                    job_type: "counter".to_string(),
                    ..Default::default()
                },
                NewJob {
                    job_type: "counter".to_string(),
                    ..Default::default()
                },
                NewJob {
                    job_type: "counter".to_string(),
                    ..Default::default()
                },
            ])
            .await
            .expect("failed to add job");

        for job_id in ids {
            wait_for_job("job to run", &test.queue, job_id).await;
        }
    }

    #[tokio::test]
    async fn worker_gets_pending_jobs_when_starting() {
        let test = TestEnvironment::new().await;

        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "counter".to_string(),
                ..Default::default()
            })
            .await
            .expect("failed to add job");

        let _worker = test.worker().build().await.expect("failed to build worker");

        wait_for_job("job to run", &test.queue, job_id).await;
    }

    #[tokio::test(start_paused = true)]
    async fn run_future_job() {
        let test = TestEnvironment::new().await;

        let _worker = test.worker().build().await.expect("failed to build worker");

        let run_at1 = test.time.now().replace_nanosecond(0).unwrap() + Duration::from_secs(10);
        let run_at2 = run_at1 + Duration::from_secs(10);

        // Schedule job 2 first, to ensure that it's actually sorting by run_at
        let job_id2 = test
            .queue
            .add_job(NewJob {
                job_type: "push_payload".to_string(),
                payload: serde_json::to_vec("job 2").unwrap(),
                run_at: Some(run_at2),
                ..Default::default()
            })
            .await
            .expect("failed to add job 2");
        event!(Level::INFO, run_at=%run_at2, id=%job_id2, "scheduled job 2");

        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "push_payload".to_string(),
                payload: serde_json::to_vec("job 1").unwrap(),
                run_at: Some(run_at1),
                ..Default::default()
            })
            .await
            .expect("failed to add job 1");
        event!(Level::INFO, run_at=%run_at1, id=%job_id, "scheduled job 1");

        tokio::time::sleep_until(test.time.instant_for_timestamp(run_at1.unix_timestamp())).await;
        let status1 = wait_for_job("job 1 to run", &test.queue, job_id).await;
        event!(Level::INFO, ?status1);
        let started_at1 = status1.started_at.expect("started_at is set on job 1");
        event!(Level::INFO, orig_run_at=%status1.orig_run_at, run_at=%run_at1, "job 1");
        assert!(status1.orig_run_at >= run_at1);
        assert!(started_at1 >= run_at1);

        tokio::time::sleep_until(test.time.instant_for_timestamp(run_at2.unix_timestamp())).await;
        let status2 = wait_for_job("job 2 to run", &test.queue, job_id2).await;
        event!(Level::INFO, ?status2);
        let started_at2 = status2.started_at.expect("started_at is set on job 2");
        event!(Level::INFO, orig_run_at=%status2.orig_run_at, run_at=%run_at2, "job 2");
        assert!(status2.orig_run_at >= run_at2);
        assert!(started_at2 >= run_at2);

        assert_eq!(test.context.get_values().await, &["job 1", "job 2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn add_multiple_future_jobs() {
        let test = TestEnvironment::new().await;

        let _worker = test.worker().build().await.expect("failed to build worker");

        let run_at1 = test.time.now().replace_nanosecond(0).unwrap() + Duration::from_secs(10);
        let run_at2 = run_at1 + Duration::from_secs(10);

        // Schedule job 2 first, to ensure that it's actually sorting by run_at
        let ids = test
            .queue
            .add_jobs(vec![
                NewJob {
                    job_type: "push_payload".to_string(),
                    payload: serde_json::to_vec("job 2").unwrap(),
                    run_at: Some(run_at2),
                    ..Default::default()
                },
                NewJob {
                    job_type: "push_payload".to_string(),
                    payload: serde_json::to_vec("job 1").unwrap(),
                    run_at: Some(run_at1),
                    ..Default::default()
                },
            ])
            .await
            .expect("Failed to add jobs");

        let job_id2 = ids[0];
        let job_id = ids[1];

        event!(Level::INFO, run_at=%run_at1, id=%job_id, "scheduled job 1");
        event!(Level::INFO, run_at=%run_at2, id=%job_id2, "scheduled job 2");

        tokio::time::sleep_until(test.time.instant_for_timestamp(run_at1.unix_timestamp())).await;
        let status1 = wait_for_job("job 1 to run", &test.queue, job_id).await;
        event!(Level::INFO, ?status1);
        let started_at1 = status1.started_at.expect("started_at is set on job 1");
        event!(Level::INFO, orig_run_at=%status1.orig_run_at, run_at=%run_at1, %started_at1, "job 1");
        assert!(status1.orig_run_at >= run_at1);
        assert!(started_at1 >= run_at1);

        tokio::time::sleep_until(test.time.instant_for_timestamp(run_at2.unix_timestamp())).await;
        let status2 = wait_for_job("job 2 to run", &test.queue, job_id2).await;
        event!(Level::INFO, ?status2);
        let started_at2 = status2.started_at.expect("started_at is set on job 2");
        event!(Level::INFO, orig_run_at=%status2.orig_run_at, run_at=%run_at2, %started_at2, "job 2");
        assert!(status2.orig_run_at >= run_at2);
        assert!(started_at2 >= run_at2);

        assert_eq!(test.context.get_values().await, &["job 1", "job 2"]);
    }

    mod retry {
        use crate::{test_util::wait_for_job_status, Retries};

        use super::*;

        #[tokio::test(start_paused = true)]
        async fn success_after_retry() {
            let test = TestEnvironment::new().await;

            let _worker = test.worker().build().await.expect("failed to build worker");

            let job_id = test
                .queue
                .add_job(NewJob {
                    job_type: "retry".to_string(),
                    payload: serde_json::to_vec(&2).unwrap(),
                    retries: Retries {
                        max_retries: 2,
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .expect("failed to add job");

            let status = wait_for_job("job to run", &test.queue, job_id).await;
            assert_eq!(status.run_info.len(), 3);
            assert!(!status.run_info[0].success);
            assert!(!status.run_info[1].success);
            assert!(status.run_info[2].success);

            assert_eq!(status.run_info[0].info.to_string(), "\"fail on try 0\"");
            assert_eq!(status.run_info[1].info.to_string(), "\"fail on try 1\"");
            assert_eq!(status.run_info[2].info.to_string(), "\"success on try 2\"");

            // Assert retry time is at least the multiplier time, but less than the additional time
            // that might be added by the randomization.
            let first_retry_time = status.run_info[1].start - status.run_info[0].start;
            event!(Level::INFO, %first_retry_time);
            assert!(first_retry_time >= Duration::from_secs(20));

            let second_retry_time = status.run_info[2].start - status.run_info[1].start;
            event!(Level::INFO, %second_retry_time);
            assert!(second_retry_time >= Duration::from_secs(40));
        }

        #[tokio::test(start_paused = true)]
        async fn exceed_max_retries() {
            let test = TestEnvironment::new().await;

            let _worker = test.worker().build().await.expect("failed to build worker");

            let job_id = test
                .queue
                .add_job(NewJob {
                    job_type: "retry".to_string(),
                    payload: serde_json::to_vec(&3).unwrap(),
                    retries: Retries {
                        max_retries: 2,
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .expect("failed to add job");

            let status =
                wait_for_job_status("job to run", &test.queue, job_id, JobState::Failed).await;
            assert_eq!(status.run_info.len(), 3);
            assert!(!status.run_info[0].success);
            assert!(!status.run_info[1].success);
            assert!(!status.run_info[2].success);

            assert_eq!(status.run_info[0].info.to_string(), "\"fail on try 0\"");
            assert_eq!(status.run_info[1].info.to_string(), "\"fail on try 1\"");
            assert_eq!(status.run_info[2].info.to_string(), "\"fail on try 2\"");
        }

        #[tokio::test]
        #[ignore]
        async fn backoff_times() {
            todo!();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_finish() {
        let mut test = TestEnvironment::new().await;

        let explicit_complete_job = JobDef::builder(
            "explicit_complete",
            |job, _context: Arc<TestContext>| async move {
                if job.current_try == 0 {
                    job.fail("explicit fail").await?;
                } else {
                    job.complete("explicit succeed").await?;
                }

                Ok::<_, crate::Error>("This should not do anything")
            },
        )
        .build();

        test.registry.add(&explicit_complete_job);

        let _worker = test.worker().build().await.expect("failed to build worker");

        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "explicit_complete".to_string(),
                ..Default::default()
            })
            .await
            .expect("failed to add job");

        let status = wait_for_job("job to run", &test.queue, job_id).await;

        assert_eq!(status.run_info.len(), 2);
        assert!(!status.run_info[0].success);
        assert!(status.run_info[1].success);

        assert_eq!(status.run_info[0].info.to_string(), "\"explicit fail\"");
        assert_eq!(status.run_info[1].info.to_string(), "\"explicit succeed\"");
    }

    #[tokio::test]
    async fn job_type_subset_with_registry() {
        let test = TestEnvironment::new().await;

        let mut run_jobs = Vec::new();
        let mut no_run_jobs = Vec::new();

        run_jobs.extend(
            test.queue
                .add_jobs(vec![
                    NewJob {
                        job_type: "counter".to_string(),
                        ..Default::default()
                    },
                    NewJob {
                        job_type: "push_payload".to_string(),
                        payload: serde_json::to_vec(&"test").unwrap(),
                        ..Default::default()
                    },
                ])
                .await
                .expect("failed to add jobs"),
        );

        no_run_jobs.push(
            test.queue
                .add_job(NewJob {
                    job_type: "sleep".to_string(),
                    payload: serde_json::to_vec(&"test").unwrap(),
                    ..Default::default()
                })
                .await
                .expect("failed to add job"),
        );

        let _worker = Worker::builder(&test.queue, test.context.clone())
            .registry(&test.registry)
            .limit_job_types(&["counter", "push_payload"])
            .build()
            .await
            .expect("failed to build worker");

        run_jobs.extend(
            test.queue
                .add_jobs(vec![
                    NewJob {
                        job_type: "counter".to_string(),
                        ..Default::default()
                    },
                    NewJob {
                        job_type: "push_payload".to_string(),
                        payload: serde_json::to_vec(&"test").unwrap(),
                        ..Default::default()
                    },
                ])
                .await
                .expect("Failed to add jobs"),
        );

        no_run_jobs.push(
            test.queue
                .add_job(NewJob {
                    job_type: "sleep".to_string(),
                    payload: serde_json::to_vec(&"test").unwrap(),
                    ..Default::default()
                })
                .await
                .expect("failed to add job"),
        );

        for job_id in run_jobs {
            event!(Level::INFO, %job_id, "checking job that should run");
            let status = wait_for_job("job to run", &test.queue, job_id).await;
            assert!(status.run_info[0].success);
        }

        for job_id in no_run_jobs {
            event!(Level::INFO, %job_id, "checking job that should not run");
            let status =
                wait_for_job_status("job to run", &test.queue, job_id, JobState::Pending).await;
            assert_eq!(status.run_info.len(), 0);
        }
    }
    #[tokio::test]
    async fn job_type_subset_with_job_list() {
        let test = TestEnvironment::new().await;

        let mut run_jobs = Vec::new();
        let mut no_run_jobs = Vec::new();

        run_jobs.extend(
            test.queue
                .add_jobs(vec![
                    NewJob {
                        job_type: "counter".to_string(),
                        ..Default::default()
                    },
                    NewJob {
                        job_type: "push_payload".to_string(),
                        payload: serde_json::to_vec(&"test").unwrap(),
                        ..Default::default()
                    },
                ])
                .await
                .expect("failed to add jobs"),
        );

        no_run_jobs.push(
            test.queue
                .add_job(NewJob {
                    job_type: "sleep".to_string(),
                    payload: serde_json::to_vec(&"test").unwrap(),
                    ..Default::default()
                })
                .await
                .expect("failed to add job"),
        );

        let _worker = Worker::builder(&test.queue, test.context.clone())
            .jobs(job_list())
            .limit_job_types(&["counter", "push_payload"])
            .build()
            .await
            .expect("failed to build worker from job list");

        run_jobs.extend(
            test.queue
                .add_jobs(vec![
                    NewJob {
                        job_type: "counter".to_string(),
                        ..Default::default()
                    },
                    NewJob {
                        job_type: "push_payload".to_string(),
                        payload: serde_json::to_vec(&"test").unwrap(),
                        ..Default::default()
                    },
                ])
                .await
                .expect("Failed to add jobs"),
        );

        no_run_jobs.push(
            test.queue
                .add_job(NewJob {
                    job_type: "sleep".to_string(),
                    payload: serde_json::to_vec(&"test").unwrap(),
                    ..Default::default()
                })
                .await
                .expect("failed to add job"),
        );

        for job_id in run_jobs {
            event!(Level::INFO, %job_id, "checking job that should run");
            let status = wait_for_job("job to run", &test.queue, job_id).await;
            assert!(status.run_info[0].success);
        }

        for job_id in no_run_jobs {
            event!(Level::INFO, %job_id, "checking job that should not run");
            let status =
                wait_for_job_status("job to run", &test.queue, job_id, JobState::Pending).await;
            assert_eq!(status.run_info.len(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn job_timeout() {
        // TODO Need to track by a specific job_run_id, not just the worker id, since
        // the next run of the job could assign it to the same worker again.
        let test = TestEnvironment::new().await;
        let _worker = test.worker().build().await.expect("failed to build worker");
        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "sleep".to_string(),
                payload: serde_json::to_vec(&10000).unwrap(),
                timeout: Duration::from_secs(5),
                retries: crate::Retries {
                    max_retries: 2,
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("failed to add job");

        let status =
            wait_for_job_status("job to fail", &test.queue, job_id, JobState::Failed).await;

        assert_eq!(status.run_info.len(), 3);
        assert!(!status.run_info[0].success);
        assert!(!status.run_info[1].success);
        assert!(!status.run_info[2].success);
        assert_eq!(status.run_info[0].info.to_string(), "\"Job expired\"");
        assert_eq!(status.run_info[1].info.to_string(), "\"Job expired\"");
        assert_eq!(status.run_info[2].info.to_string(), "\"Job expired\"");
    }

    // TODO Run this in virtual time once https://github.com/tokio-rs/tokio/pull/5115 is merged.
    #[tokio::test]
    async fn manual_heartbeat() {
        let mut test = TestEnvironment::new().await;
        let job_def = JobDef::builder(
            "manual_heartbeat",
            |job, _context: Arc<TestContext>| async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                job.heartbeat().await?;
                tokio::time::sleep(Duration::from_millis(750)).await;
                Ok::<_, crate::Error>(())
            },
        )
        .build();

        test.registry.add(&job_def);
        let _worker = test.worker().build().await.expect("failed to build worker");

        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "manual_heartbeat".to_string(),
                retries: crate::Retries {
                    max_retries: 0,
                    ..Default::default()
                },
                timeout: Duration::from_secs(1),
                ..Default::default()
            })
            .await
            .expect("failed to add job");

        wait_for_job("job to succeed", &test.queue, job_id).await;
    }

    // TODO Run this in virtual time once https://github.com/tokio-rs/tokio/pull/5115 is merged.
    #[tokio::test]
    async fn auto_heartbeat() {
        let mut test = TestEnvironment::new().await;
        let job_def = JobDef::builder(
            "auto_heartbeat",
            |_job, _context: Arc<TestContext>| async move {
                tokio::time::sleep(Duration::from_millis(2500)).await;
                Ok::<_, crate::Error>(())
            },
        )
        .autoheartbeat(true)
        .build();

        test.registry.add(&job_def);
        let _worker = test.worker().build().await.expect("failed to build worker");

        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "auto_heartbeat".to_string(),
                retries: crate::Retries {
                    max_retries: 0,
                    ..Default::default()
                },
                timeout: Duration::from_secs(2),
                ..Default::default()
            })
            .await
            .expect("failed to add job");

        wait_for_job("job to succeed", &test.queue, job_id).await;
    }

    #[tokio::test]
    async fn job_priority() {
        let test = TestEnvironment::new().await;

        let now = test.time.now();

        // Add two job. The low priority job has an earlier run_at time so it would normally run first,
        // but the high priority job should actually run first due to running in priority order.
        let low_prio = test
            .queue
            .add_job(NewJob {
                job_type: "push_payload".to_string(),
                payload: serde_json::to_vec("low").unwrap(),
                priority: 1,
                run_at: Some(now - Duration::from_secs(10)),
                ..Default::default()
            })
            .await
            .expect("adding low priority job");

        let high_prio = test
            .queue
            .add_job(NewJob {
                job_type: "push_payload".to_string(),
                payload: serde_json::to_vec("high").unwrap(),
                priority: 2,
                run_at: Some(now - Duration::from_secs(5)),
                ..Default::default()
            })
            .await
            .expect("adding high priority job");

        let _worker = test.worker().build().await.expect("failed to build worker");

        let status_high = wait_for_job("high priority job to run", &test.queue, high_prio).await;
        let status_low = wait_for_job("low priority job to run", &test.queue, low_prio).await;

        let high_started_at = status_high.started_at.expect("high priority job started");
        let low_started_at = status_low.started_at.expect("low priority job started");
        event!(Level::INFO, high_started_at=%high_started_at, low_started_at=%low_started_at, "job start times");
        assert!(high_started_at <= low_started_at);

        assert_eq!(test.context.get_values().await, vec!["high", "low"]);
    }

    #[tokio::test]
    async fn checkpoint() {
        let mut test = TestEnvironment::new().await;
        let job_def = JobDef::builder(
            "checkpoint_job",
            |job, _context: Arc<TestContext>| async move {
                let payload = job.json_payload::<String>().unwrap();
                event!(Level::INFO, %job, %payload, "running checkpoint job");
                match job.current_try {
                    0 => {
                        assert_eq!(payload, "initial", "checkpoint when loaded");
                        job.checkpoint_json("first").await.unwrap();
                        Err("fail 1")
                    }
                    1 => {
                        assert_eq!(payload, "first", "checkpoint after first run");
                        job.checkpoint_json("second").await.unwrap();
                        Err("fail 2")
                    }
                    2 => {
                        assert_eq!(payload, "second", "checkpoint after second run");
                        job.checkpoint_json("third").await.unwrap();
                        Ok("success")
                    }
                    _ => panic!("unexpected try number"),
                }
            },
        )
        .build();

        test.registry.add(&job_def);
        let _worker = test.worker().build().await.expect("failed to build worker");

        let job_id = test
            .queue
            .add_job(NewJob {
                job_type: "checkpoint_job".to_string(),
                payload: serde_json::to_vec("initial").unwrap(),
                retries: crate::Retries {
                    max_retries: 3,
                    backoff_multiplier: 1.0,
                    backoff_initial_interval: Duration::from_millis(1),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("failed to add job");

        let status = wait_for_job("job to succeed", &test.queue, job_id).await;

        assert_eq!(status.state, JobState::Succeeded);
        assert_eq!(status.run_info.len(), 3);
        assert!(!status.run_info[0].success);
        assert!(!status.run_info[1].success);
        assert!(status.run_info[2].success);
        assert_eq!(status.run_info[0].info.to_string(), "\"fail 1\"");
        assert_eq!(status.run_info[1].info.to_string(), "\"fail 2\"");
        assert_eq!(status.run_info[2].info.to_string(), "\"success\"");
    }

    mod concurrency {
        use super::*;

        #[tokio::test]
        async fn limit_to_max_concurrency() {
            let test = TestEnvironment::new().await;

            let mut jobs = Vec::new();
            for _ in 0..10 {
                let job_id = test
                    .queue
                    .add_job(NewJob {
                        job_type: "max_count".to_string(),
                        ..Default::default()
                    })
                    .await
                    .expect("Adding job");
                jobs.push(job_id);
            }

            let _worker = test
                .worker()
                .max_concurrency(5)
                .build()
                .await
                .expect("failed to build worker");

            for (i, job_id) in jobs.into_iter().enumerate() {
                wait_for_job(format!("job {i} to succeed"), &test.queue, job_id).await;
            }

            assert_eq!(test.context.max_count().await, 5);
        }

        #[tokio::test]
        async fn weighted_jobs() {
            let test = TestEnvironment::new().await;

            let mut jobs = Vec::new();
            for _ in 0..10 {
                let job_id = test
                    .queue
                    .add_job(NewJob {
                        job_type: "max_count".to_string(),
                        weight: 3,
                        ..Default::default()
                    })
                    .await
                    .expect("Adding job");
                jobs.push(job_id);
            }

            let worker = test
                .worker()
                .max_concurrency(10)
                .build()
                .await
                .expect("failed to build worker");

            for job_id in jobs {
                wait_for_job("job to succeed", &test.queue, job_id).await;
            }

            // With a weight of 3, there should be at most three jobs (10 / 3) running at once.
            assert_eq!(test.context.max_count().await, 3);
            let counts = worker.counts();

            assert_eq!(counts.started, 10);
            assert_eq!(counts.finished, 10);
        }

        #[tokio::test]
        async fn fetches_again_at_min_concurrency() {
            let mut test = TestEnvironment::new().await;

            let ms_job = JobDef::builder("ms_job", |job, context: Arc<TestContext>| async move {
                let start_time = context.start_time.elapsed().as_millis() as u32;
                let sleep_time = job.json_payload::<u64>().unwrap();
                tokio::time::sleep(Duration::from_millis(sleep_time)).await;
                Ok::<_, String>(start_time)
            })
            .build();

            test.registry.add(&ms_job);

            let mut jobs = Vec::new();
            for i in 0..20 {
                let time = (i + 1) * 100;
                let job_id = test
                    .queue
                    .add_job(NewJob {
                        job_type: "ms_job".to_string(),
                        payload: serde_json::to_vec(&time).unwrap(),
                        timeout: Duration::from_secs(20 * 60),
                        ..Default::default()
                    })
                    .await
                    .expect("Adding job");
                jobs.push(job_id);
            }

            let _worker = test
                .worker()
                .min_concurrency(6)
                .max_concurrency(10)
                .build()
                .await
                .expect("failed to build worker");

            let mut statuses = Vec::new();
            for job_id in jobs {
                statuses.push(wait_for_job("job to succeed", &test.queue, job_id).await);
            }

            let times = statuses
                .iter()
                .map(|s| serde_json::from_str::<u32>(s.run_info[0].info.get()).unwrap())
                .collect::<Vec<_>>();
            event!(Level::INFO, ?times);
            println!("{:?}", times);

            // First 10 jobs should all start at same time.
            let batch1_time = times[0];
            for i in 1..10 {
                assert!(times[i] - batch1_time <= 1);
            }

            // Next 5 jobs should start together
            let batch2_time = times[10];
            assert!(batch2_time - batch1_time >= 300);
            for i in 11..15 {
                assert!(times[i] - batch2_time <= 1);
            }

            // Final 5 jobs should start together
            let batch3_time = times[15];
            assert!(batch3_time - batch2_time >= 300);
            for i in 16..20 {
                assert!(times[i] - batch3_time <= 1);
            }
        }
    }

    #[tokio::test]
    async fn shutdown() {
        let jobs = (0..20)
            .map(|i| {
                let timeout = i * 75;

                NewJob {
                    job_type: "sleep".to_string(),
                    payload: serde_json::to_vec(&timeout).unwrap(),
                    timeout: Duration::from_secs(5),
                    retries: crate::Retries {
                        max_retries: 2,
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>();

        let test = TestEnvironment::new().await;

        let job_ids = test.queue.add_jobs(jobs).await.expect("Adding jobs");

        let _worker = test
            .worker()
            .min_concurrency(7)
            .max_concurrency(10)
            .build()
            .await
            .expect("failed to build worker");

        tokio::time::sleep(Duration::from_millis(250)).await;

        event!(Level::INFO, "shutting down");
        test.queue
            .close(Duration::from_secs(5))
            .await
            .expect("failed to close queue");

        let mut successful = 0;
        let mut pending = 0;
        for job_id in job_ids {
            let status = test
                .queue
                .get_job_status(job_id)
                .await
                .expect("getting job status");
            // Jobs should either be done or not started yet. Nothing should be left hanging in a
            // running state.

            if status.state == JobState::Succeeded {
                successful += 1;
            } else if status.state == JobState::Pending {
                pending += 1;
            }

            event!(Level::INFO, ?status);
            assert!(status.state == JobState::Succeeded || status.state == JobState::Pending);
        }

        // We should have run at least some of the jobs, but not all of them yet.
        event!(Level::INFO, %successful, %pending);
        assert!(successful > 0);
        assert!(pending > 0);
    }

    mod unimplemented {
        #[tokio::test]
        #[ignore = "not implemented yet"]
        async fn remove_jobs() {
            unimplemented!();
        }

        #[tokio::test]
        #[ignore = "not implemented yet"]
        async fn clear_jobs() {
            unimplemented!();
        }

        #[tokio::test]
        #[ignore = "not implemented yet"]
        async fn recurring_jobs() {
            unimplemented!();
        }
    }
}
