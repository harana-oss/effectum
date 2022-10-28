use ahash::HashMap;
use rusqlite::named_params;
use std::fmt::Debug;
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicU16, Ordering};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::job::Job;
use crate::job_registry::{JobDef, JobRegistry};
use crate::shared_state::SharedState;
use crate::worker_list::ListeningWorker;
use crate::{Error, Queue, Result, SmartString};

type WorkerId = u64;

struct CancellableTask {
    close_tx: oneshot::Sender<()>,
    join_handle: JoinHandle<()>,
}

pub struct Worker {
    pub id: WorkerId,
    worker_list_task: Option<CancellableTask>,
}

impl Worker {
    pub async fn unregister(mut self, timeout: Option<std::time::Duration>) -> Result<()> {
        if let Some(task) = self.worker_list_task.take() {
            task.close_tx.send(()).ok();
            if let Some(timeout) = timeout {
                tokio::time::timeout(timeout, task.join_handle)
                    .await
                    .map_err(|_| Error::Timeout)??;
            } else {
                task.join_handle.await?;
            }
        }
        Ok(())
    }

    pub fn builder<'a, CONTEXT>(
        registry: &'a JobRegistry<CONTEXT>,
        queue: &'a Queue,
        context: CONTEXT,
    ) -> WorkerBuilder<'a, CONTEXT>
    where
        CONTEXT: Send + Sync + Debug + Clone + 'static,
    {
        WorkerBuilder::new(registry, queue, context)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(task) = self.worker_list_task.take() {
            task.close_tx.send(()).ok();
            tokio::spawn(task.join_handle);
        }
    }
}

pub struct WorkerBuilder<'a, CONTEXT>
where
    CONTEXT: Send + Sync + Debug + Clone + 'static,
{
    /// The job registry from which this worker should take its job functions.
    registry: &'a JobRegistry<CONTEXT>,
    queue: &'a Queue,
    /// The context value to send to the worker's jobs.
    context: CONTEXT,
    /// Limit the job types this worker will run. Defaults to all job types in the registry.
    jobs: Vec<SmartString>,
    /// Fetch new jobs when the number of running jobs drops to this number. Defaults to
    /// max_concurrency / 2.
    min_concurrency: Option<u16>,
    /// The maximum number of jobs that can be run concurrently.
    /// Defaults to the highest weight of any job in the registry, accounting for `jobs` if set.
    max_concurrency: Option<u16>,
}

impl<'a, CONTEXT> WorkerBuilder<'a, CONTEXT>
where
    CONTEXT: Send + Sync + Debug + Clone + 'static,
{
    pub fn new(registry: &'a JobRegistry<CONTEXT>, queue: &'a Queue, context: CONTEXT) -> Self {
        Self {
            registry,
            queue,
            context,
            jobs: Vec::new(),
            min_concurrency: None,
            max_concurrency: None,
        }
    }

    pub fn job_types(&mut self, job_types: &[impl AsRef<str>]) -> &mut Self {
        self.jobs = job_types
            .iter()
            .map(|s| {
                assert!(
                    self.registry.jobs.contains_key(s.as_ref()),
                    "Job type {} not found in registry",
                    s.as_ref()
                );

                SmartString::from(s.as_ref())
            })
            .collect();
        self
    }

    pub fn min_concurrency(&mut self, min_concurrency: u16) -> &mut Self {
        assert!(min_concurrency > 0);
        self.min_concurrency = Some(min_concurrency);
        self
    }

    pub fn max_concurrency(&mut self, max_concurrency: u16) -> &mut Self {
        assert!(max_concurrency > 0);
        self.max_concurrency = Some(max_concurrency);
        self
    }

    pub async fn build(self) -> Result<Worker> {
        let job_list = if self.jobs.is_empty() {
            self.registry.jobs.keys().cloned().collect()
        } else {
            self.jobs
        };

        let job_defs = job_list
            .iter()
            .filter_map(|job| {
                self.registry
                    .jobs
                    .get(job)
                    .map(|job_def| (job.clone(), job_def.clone()))
            })
            .collect();

        let jobs_max_concurrency = job_list
            .iter()
            .map(|job| self.registry.jobs.get(job).map(|d| d.weight).unwrap_or(1))
            .max()
            .unwrap_or(1);

        let max_concurrency = self
            .max_concurrency
            .unwrap_or(jobs_max_concurrency)
            .max(jobs_max_concurrency);

        let min_concurrency = self.min_concurrency.unwrap_or(max_concurrency / 2).max(1);

        let (close_tx, close_rx) = oneshot::channel();

        let mut workers = self.queue.state.workers.write().await;
        let listener = workers.add_worker(&job_list);
        drop(workers);

        let worker_id = listener.id;
        let worker_internal = WorkerInternal {
            listener,
            running_jobs: Arc::new(RunningJobs {
                count: AtomicU16::new(0),
                job_finished: Notify::new(),
            }),
            job_list: job_list.into_iter().map(String::from).collect(),
            job_defs: Arc::new(job_defs),
            queue: self.queue.state.clone(),
            context: self.context,
            min_concurrency,
            max_concurrency,
        };

        let join_handle = tokio::spawn(worker_internal.run(close_rx));

        Ok(Worker {
            id: worker_id,
            worker_list_task: Some(CancellableTask {
                close_tx,
                join_handle,
            }),
        })
    }
}

struct RunningJobs {
    count: AtomicU16,
    job_finished: Notify,
}

struct WorkerInternal<CONTEXT>
where
    CONTEXT: Send + Sync + Debug + Clone + 'static,
{
    listener: Arc<ListeningWorker>,
    queue: SharedState,
    job_list: Vec<String>,
    job_defs: Arc<HashMap<SmartString, JobDef<CONTEXT>>>,
    running_jobs: Arc<RunningJobs>,
    context: CONTEXT,
    min_concurrency: u16,
    max_concurrency: u16,
}

impl<CONTEXT> WorkerInternal<CONTEXT>
where
    CONTEXT: Send + Sync + Debug + Clone + 'static,
{
    async fn run(self, mut close_rx: oneshot::Receiver<()>) {
        let mut global_close_rx = self.queue.close.clone();
        loop {
            let mut running_jobs = self.running_jobs.count.load(Ordering::Relaxed);
            if running_jobs < self.min_concurrency {
                self.run_ready_jobs().await;
                running_jobs = self.running_jobs.count.load(Ordering::Relaxed);
            }

            let grab_new_jobs = running_jobs < self.min_concurrency;

            tokio::select! {
                biased;
                _ = &mut close_rx => {
                    self.shutdown().await;
                    break;
                }
                _ = global_close_rx.changed() => {
                    self.shutdown().await;
                    break;
                }
                _ = self.listener.notify_task_ready.notified(), if grab_new_jobs  => {}
                _ = self.running_jobs.job_finished.notified() => {}
            }
        }
    }

    async fn shutdown(&self) -> Result<()> {
        // TODO Wait for jobs to shut down

        let mut workers = self.queue.workers.write().await;
        workers.remove_worker(self.listener.id)
    }

    async fn run_ready_jobs(&self) -> Result<()> {
        let running_count = self.running_jobs.count.load(Ordering::Relaxed);
        let max_concurrency = self.max_concurrency;
        let max_jobs = max_concurrency - running_count;
        let job_types = self
            .job_list
            .iter()
            .map(|s| rusqlite::types::Value::from(s.clone()))
            .collect::<Vec<_>>();

        let queue = self.queue.clone();
        let job_defs = self.job_defs.clone();
        let running_jobs = self.running_jobs.clone();
        let worker_id = self.listener.id;

        let ready_jobs = self
            .queue
            .write_db(move |db| {
                let tx = db.transaction()?;

                let mut ready_jobs = Vec::with_capacity(max_jobs as usize);

                {
                    let now = OffsetDateTime::now_utc();
                    let now_timestamp = now.unix_timestamp();
                    let mut stmt = tx.prepare_cached(
                        r##"SELECT job_id, external_id, priority, job_type, current_try,
                            COALESCE(checkpointed_payload, payload) as payload,
                            default_timeout,
                            heartbeat_increment,
                            backoff_multiplier,
                            backoff_randomization,
                            backoff_initial_interval,
                            max_retries
                        FROM active_jobs
                        WHERE job_type in rarray($job_types) AND run_at <= $now
                        ORDER BY priority DESC, run_at
                        LIMIT $limit"##,
                    )?;

                    struct JobResult {
                        job_id: i64,
                        external_id: Uuid,
                        priority: i32,
                        job_type: String,
                        current_try: i32,
                        payload: Option<Vec<u8>>,
                        default_timeout: i32,
                        heartbeat_increment: i32,
                        backoff_multiplier: f64,
                        backoff_randomization: f64,
                        backoff_initial_interval: i32,
                        max_retries: i32,
                    }

                    let jobs = stmt.query_map(
                        named_params! {
                            "$job_types": Rc::new(job_types),
                            "$now": now_timestamp,
                            "$limit": max_jobs,
                        },
                        |row| {
                            let job_id: i64 = row.get(0)?;
                            let external_id: Uuid = row.get(1)?;
                            let priority: i32 = row.get(2)?;
                            let job_type: String = row.get(3)?;
                            let current_try: i32 = row.get(4)?;
                            let payload: Option<Vec<u8>> = row.get(5)?;
                            let default_timeout: i32 = row.get(6)?;
                            let heartbeat_increment: i32 = row.get(7)?;
                            let backoff_multiplier: f64 = row.get(8)?;
                            let backoff_randomization: f64 = row.get(9)?;
                            let backoff_initial_interval: i32 = row.get(10)?;
                            let max_retries: i32 = row.get(11)?;

                            Ok(JobResult {
                                job_id,
                                priority,
                                job_type,
                                current_try,
                                payload,
                                default_timeout,
                                external_id,
                                heartbeat_increment,
                                backoff_multiplier,
                                backoff_randomization,
                                backoff_initial_interval,
                                max_retries,
                            })
                        },
                    )?;

                    let mut set_running = tx.prepare_cached(
                        r##"UPDATE active_jobs
                        SET worker_id=$worker_id, started_at=$now, expires_at=$expiration
                        WHERE job_id=$job_id"##,
                    )?;

                    let mut running_count = running_count;
                    for job in jobs {
                        let job = job?;
                        let weight = job_defs
                            .get(job.job_type.as_str())
                            .map(|j| j.weight)
                            .unwrap_or(1);

                        if running_count + weight > max_concurrency {
                            break;
                        }

                        let expiration = now_timestamp + job.default_timeout as i64;

                        set_running.execute(named_params! {
                            "$job_id": job.job_id,
                            "$worker_id": worker_id,
                            "$now": now,
                            "$expiration": expiration
                        })?;

                        running_count =
                            running_jobs.count.fetch_add(weight, Ordering::Relaxed) + weight;

                        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                        let job = Job {
                            id: job.external_id,
                            job_id: job.job_id,
                            worker_id,
                            heartbeat_increment: job.heartbeat_increment,
                            job_type: job.job_type,
                            payload: job.payload.unwrap_or_default(),
                            priority: job.priority,
                            expires: Arc::new(AtomicI64::new(expiration)),
                            start_time: now,
                            current_try: job.current_try,
                            backoff_multiplier: job.backoff_multiplier,
                            backoff_randomization: job.backoff_randomization,
                            backoff_initial_interval: job.backoff_initial_interval,
                            max_retries: job.max_retries,
                            done: Some(done_tx),
                            queue: queue.clone(),
                        };

                        ready_jobs.push((job, done_rx));
                    }
                }

                tx.commit()?;
                Ok(ready_jobs)
            })
            .await?;

        for job in ready_jobs {
            self.run_job(job).await?;
        }

        Ok(())
    }

    async fn run_job(
        &self,
        (job, mut done): (Job, tokio::sync::oneshot::Receiver<()>),
    ) -> Result<()> {
        let job_def = self
            .job_defs
            .get(job.job_type.as_str())
            .expect("Got job for unsupported type");

        let worker_id = self.listener.id;
        let job_id = job.job_id;
        let weight = job_def.weight;
        let running = self.running_jobs.clone();
        let heartbeat_increment = job.heartbeat_increment;
        let expires = job.expires.clone();
        let queue = self.queue.clone();
        let autoheartbeat = job_def.autoheartbeat;

        (job_def.runner)(job, self.context.clone());

        tokio::spawn(async move {
            if autoheartbeat && heartbeat_increment > 0 {
                loop {
                    tokio::select! {
                        _ = wait_for_next_autoheartbeat(heartbeat_increment, &expires) => {
                            let new_time =
                                crate::job::send_heartbeat(job_id, worker_id, heartbeat_increment, &queue).await;

                            // TODO log error
                            if let Ok(new_time) = new_time {
                                expires.store(new_time.unix_timestamp(), Ordering::Relaxed);
                            }
                        }
                        _ = &mut done => {
                            break;
                        }
                    }
                }
            } else {
                done.await.ok();
            }

            // Do this in a separate task from the job runner so that even if something goes horribly wrong
            // we'll still be able to update the internal counts.
            running.count.fetch_sub(weight, Ordering::Relaxed);
            running.job_finished.notify_one();
        });

        Ok(())
    }
}

async fn wait_for_next_autoheartbeat(heartbeat_increment: i32, expires: &Arc<AtomicI64>) {
    let before = (heartbeat_increment.min(30) / 2) as i64;
    let next_heartbeat_time = expires.load(Ordering::Relaxed) - before;

    let time_from_now = next_heartbeat_time - OffsetDateTime::now_utc().unix_timestamp();
    let instant = Instant::now() + std::time::Duration::from_secs(time_from_now.max(0) as u64);

    tokio::time::sleep_until(instant).await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn shutdown() {
        todo!();
    }
}
