# Effectum

A Rust job queue library, based on SQLite so it doesn't depend on any other services.

Currently this is just a library embeddable into Rust applications, but future goals include bindings into other languages
and the ability to run as a standalone server, accessible by HTTP and gRPC. This will be designed so that a product can
start with the embedded version to use minimal infrastructure, and then move to the server version with minimal changes when
the time comes to scale out.

```rust
use effectum::{Error, Job, JobState, JobRunner, RunningJob, Queue, Worker};

#[derive(Debug)]
pub struct JobContext {
   // database pool or other things here
}

#[derive(Serialize, Deserialize)]
struct RemindMePayload {
  email: String,
  message: String,
}

async fn remind_me_job(job: RunningJob, context: Arc<JobContext>) -> Result<(), Error> {
    let payload: RemindMePayload = job.json_payload()?;
    // do something with the job
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
  // Create a queue
  let queue = Queue::new(&PathBuf::from("effectum.db")).await?;

  // Define a type job for the queue.
  let a_job = JobRunner::builder("remind_me", remind_me_job).build();

  let context = Arc::new(JobContext{
    // database pool or other things here
  });

  // Submit a job to the queue.
  let job_id = Job::builder("remind_me")
    .run_at(time::OffsetDateTime::now_utc() + std::time::Duration::from_secs(3600))
    .json_payload(&RemindMePayload {
        email: "me@example.com".to_string(),
        message: "Time to go!".to_string()
    })?
    .add_to(&queue)
    .await?;

  // Job is waiting on available workers. 
  let status = queue.get_job_status(job_id).await?;
  assert_eq!(status.state, JobState::Pending);

  // Create a worker to handle remind_me jobs.
  let worker = Worker::builder(&queue, context)
    .max_concurrency(10)
    .jobs([a_job])
    .build()
    .await?;

  // Job has now been executed. 
  let status = queue.get_job_status(job_id).await?;
  assert_eq!(status.state, JobState::Succeeded);

  Ok(())
}
```

[Changelog](https://github.com/dimfeld/effectum/blob/master/effectum/CHANGELOG.md)

[Full Development Notes](https://imfeld.dev/notes/projects_effectum)

# Roadmap

## Released

- Multiple job types
- Jobs can be added with higher priority to "skip the line"
- Workers can run multiple jobs concurrently
- Schedule jobs in the future
- Automatically retry failed jobs, with exponential backoff
- Checkpoints to allow smart resumption of a job if it fails midway through.
- Immediately schedule a retry for jobs that were running when the process restarts unexpectedly
- Cancel or modify pending jobs
- Support for recurring jobs

## Soon

- Optional sweeper to prevent "done" job data from building up indefinitely

## Later

- Node.js bindings
- Run as a standalone server over gRPC
- Helpers for communicating with the queue via the outbox pattern.
