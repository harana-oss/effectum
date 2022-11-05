use std::{borrow::Borrow, fmt::Debug, fmt::Display, panic::AssertUnwindSafe, sync::Arc};

use ahash::HashMap;
use futures::{Future, FutureExt};
use serde::Serialize;
use tracing::{event, span, Instrument, Level};

use crate::{job::Job, worker::log_error, SmartString};

pub(crate) type JobFn<CONTEXT> =
    Arc<dyn Fn(Job, CONTEXT) -> tokio::task::JoinHandle<()> + Send + Sync + 'static>;

/// A list of jobs that can be run by a worker.
pub struct JobRegistry<CONTEXT>
where
    CONTEXT: Send + Sync + Debug + Clone + 'static,
{
    pub(crate) jobs: HashMap<SmartString, JobDef<CONTEXT>>,
}

impl<CONTEXT> JobRegistry<CONTEXT>
where
    CONTEXT: Send + Sync + Debug + Clone + 'static,
{
    /// Create a new job registry from a list of [JobDefs](JobDef).
    pub fn new<JOBLIST>(jobs: JOBLIST) -> JobRegistry<CONTEXT>
    where
        JOBLIST: IntoIterator,
        JOBLIST::Item: Borrow<JobDef<CONTEXT>>,
    {
        let jobs = jobs
            .into_iter()
            .map(|d| {
                let d = d.borrow().to_owned();
                (d.name.clone(), d)
            })
            .collect();

        JobRegistry { jobs }
    }

    /// Add a [JobDef] to an existing registry.
    pub fn add(&mut self, job: &JobDef<CONTEXT>) {
        self.jobs
            .entry(job.name.clone())
            .and_modify(|_| {
                panic!("Job {} already exists", job.name);
            })
            .or_insert_with(|| job.clone());
    }
}

/// A definition of a job, including the name of the job, the function that runs the job, and
/// other settings.
///
/// The function that runs the job should be an `async` function that takes a [Job] and a context object.
/// All jobs for a particular worker must have the same context type.
///
/// The function can be either a normal function or a closure.
///
/// ```
/// # use prefect::*;
/// # use std::sync::Arc;
/// #[derive(Debug)]
/// pub struct JobContext {
///   // database pool or other things here
/// }
///
/// let job = JobDef::builder("a_job", |job: Job, context: Arc<JobContext>| async move {
///   // do some work
///   Ok::<_, Error>("optional info about the success")
/// }).build();
///
/// async fn another_job(job: Job, context: Arc<JobContext>) -> Result<String, Error> {
///   // do some work
///   Ok("optional info about the success".to_string())
/// }
///
/// let another_job = JobDef::builder("another_job", another_job)
///     .autoheartbeat(true)
///     .build();
/// ```
#[derive(Clone)]
pub struct JobDef<CONTEXT>
where
    CONTEXT: Send + Debug + Clone + 'static,
{
    pub(crate) name: SmartString,
    pub(crate) runner: JobFn<CONTEXT>,
    pub(crate) autoheartbeat: bool,
}

impl<CONTEXT> JobDef<CONTEXT>
where
    CONTEXT: Send + Sync + Debug + Clone + 'static,
{
    /// Create a new [JobDef], passing all the possible fields. Generally it's easier to use
    /// [JobDef::builder].
    pub fn new<F, Fut, T, E>(
        name: impl Into<SmartString>,
        runner: F,
        autoheartbeat: bool,
    ) -> JobDef<CONTEXT>
    where
        F: Fn(Job, CONTEXT) -> Fut + Send + Sync + Clone + 'static,
        CONTEXT: Send + Debug + Clone + 'static,
        Fut: Future<Output = Result<T, E>> + Send + Sync,
        T: Send + Sync + Debug + Serialize + 'static,
        E: Send + Display + 'static,
    {
        let f = move |job: Job, context: CONTEXT| {
            let runner = runner.clone();
            tokio::spawn(async move {
                let result = {
                    let span = span!(Level::INFO, "run_job", %job);
                    AssertUnwindSafe(runner(job.clone(), context).instrument(span))
                        .catch_unwind()
                        .await
                };

                let explicitly_finished = job.is_done().await;
                event!(Level::DEBUG, ?job, %explicitly_finished, now=%job.queue.time.now(), "done");
                match result {
                    Err(e) => {
                        let msg = if let Some(s) = e.downcast_ref::<&str>() {
                            s.to_string()
                        } else if let Some(s) = e.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "Panic".to_string()
                        };

                        if explicitly_finished {
                            event!(Level::ERROR, %msg, "Job panicked after it was completed");
                        } else {
                            log_error(job.fail(msg).await);
                        }
                    }
                    Ok(Ok(info)) => {
                        if !explicitly_finished {
                            log_error(job.complete(info).await);
                        }
                    }
                    Ok(Err(e)) => {
                        if explicitly_finished {
                            event!(
                                Level::ERROR,
                                err = %e,
                                "Job returned error after it was completed"
                            );
                        } else {
                            let msg = e.to_string();
                            log_error(job.fail(msg).await);
                        }
                    }
                }
            })
        };

        JobDef {
            name: name.into(),
            runner: Arc::new(f),
            autoheartbeat,
        }
    }

    /// Create a [JobDefBuilder] for this job.
    pub fn builder<F, Fut, T, E>(name: impl Into<SmartString>, runner: F) -> JobDefBuilder<CONTEXT>
    where
        F: Fn(Job, CONTEXT) -> Fut + Send + Sync + Clone + 'static,
        CONTEXT: Send + Debug + Clone + 'static,
        Fut: Future<Output = Result<T, E>> + Send + Sync,
        T: Send + Sync + Debug + Serialize + 'static,
        E: Send + Display + 'static,
    {
        let def = JobDef::new(name, runner, false);
        JobDefBuilder { def }
    }
}

/// A builder object for a [JobDef].
pub struct JobDefBuilder<CONTEXT>
where
    CONTEXT: Send + Debug + Clone + 'static,
{
    def: JobDef<CONTEXT>,
}

impl<CONTEXT> JobDefBuilder<CONTEXT>
where
    CONTEXT: Send + Debug + Clone + 'static,
{
    /// Set whether the job should automatically send heartbeats while it runs.
    pub fn autoheartbeat(mut self, autoheartbeat: bool) -> Self {
        self.def.autoheartbeat = autoheartbeat;
        self
    }

    /// Consume the builder, returning a [JobDef].
    pub fn build(self) -> JobDef<CONTEXT> {
        self.def
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        test_util::{TestContext, TestEnvironment},
        Job,
    };

    use super::{JobDef, JobRegistry};

    async fn test_job(_job: Job, _context: ()) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn create_job_from_fn() {
        JobDef::new("test", test_job, false);
    }

    mod registry_joblist {
        use super::*;

        #[tokio::test]
        async fn slice_of_objects() {
            let job = JobDef::new("test", test_job, false);
            JobRegistry::new(&[job]);
        }

        #[tokio::test]
        async fn array_of_objects() {
            let job = JobDef::new("test", test_job, false);
            JobRegistry::new([job]);
        }

        #[tokio::test]
        async fn array_of_refs() {
            let job = JobDef::new("test", test_job, false);
            JobRegistry::new([&job]);
        }

        #[tokio::test]
        async fn vec_of_objects() {
            let job = JobDef::new("test", test_job, false);
            JobRegistry::new(vec![job]);
        }

        #[tokio::test]
        async fn vec_of_refs() {
            let job = JobDef::new("test", test_job, false);
            JobRegistry::new(vec![&job]);
        }
    }

    #[tokio::test]
    #[should_panic]
    async fn disallow_adding_same_job_type_twice() {
        let mut test = TestEnvironment::new().await;

        let job = JobDef::builder("counter", |_, _context: Arc<TestContext>| async {
            Ok::<_, String>(())
        })
        .build();
        test.registry.add(&job);
    }
}
