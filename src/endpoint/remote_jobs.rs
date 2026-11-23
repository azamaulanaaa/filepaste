use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::distr::Alphanumeric;
use rand::distr::Distribution;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The time that the store keeps a finished job.
pub const JOB_RETENTION: Duration = Duration::from_hours(1);

/// The number of random characters in a job ID.
const JOB_ID_LENGTH: usize = 22;

/// The number of random characters in a job token.
const JOB_TOKEN_LENGTH: usize = 43;

/// The state of a remote upload job.
#[derive(Debug)]
pub enum JobState {
    /// The job reads the target body or writes the stored file.
    Fetching,
    /// The job stored the file.
    Done { url: String },
    /// The job stopped before it stored the file.
    Failed { error: String },
}

impl JobState {
    /// Return the state name for a status response.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fetching => "fetching",
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
        }
    }
}

/// A copy of the job state for a status response.
#[derive(Debug)]
pub struct JobStatus {
    pub state: JobState,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
}

struct JobInner {
    state: JobState,
    received_bytes: u64,
    total_bytes: Option<u64>,
    finished_at: Option<Instant>,
}

/// One remote upload job.
pub struct Job {
    token: String,
    path: PathBuf,
    cancel: watch::Sender<bool>,
    inner: Mutex<JobInner>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Job {
    fn new(token: String, path: PathBuf, total_bytes: Option<u64>) -> Self {
        Self {
            token,
            path,
            cancel: watch::channel(false).0,
            inner: Mutex::new(JobInner {
                state: JobState::Fetching,
                received_bytes: 0,
                total_bytes,
                finished_at: None,
            }),
            task: Mutex::new(None),
        }
    }

    /// Return the job token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Return true when the candidate matches the job token.
    pub fn token_matches(&self, candidate: &str) -> bool {
        let expected = self.token.as_bytes();
        let actual = candidate.as_bytes();
        if expected.len() != actual.len() {
            return false;
        }
        expected
            .iter()
            .zip(actual)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }

    /// Return the storage path of the job.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the number of bytes that the job received.
    pub fn received_bytes(&self) -> u64 {
        self.inner.lock().expect("job lock poisoned").received_bytes
    }

    /// Return a copy of the job state.
    pub fn status(&self) -> JobStatus {
        let inner = self.inner.lock().expect("job lock poisoned");
        let state = match &inner.state {
            JobState::Fetching => JobState::Fetching,
            JobState::Done { url } => JobState::Done { url: url.clone() },
            JobState::Failed { error } => JobState::Failed {
                error: error.clone(),
            },
        };
        JobStatus {
            state,
            received_bytes: inner.received_bytes,
            total_bytes: inner.total_bytes,
        }
    }

    /// Record the number of bytes that the job received.
    pub fn mark_received(&self, received_bytes: u64) {
        self.inner.lock().expect("job lock poisoned").received_bytes = received_bytes;
    }

    /// Record the target body size when the target sends it.
    pub fn set_total_bytes(&self, total_bytes: Option<u64>) {
        self.inner.lock().expect("job lock poisoned").total_bytes = total_bytes;
    }

    /// Mark the job as done.
    pub fn finish_done(&self, url: String) {
        let mut inner = self.inner.lock().expect("job lock poisoned");
        inner.state = JobState::Done { url };
        inner.finished_at = Some(Instant::now());
    }

    /// Mark the job as failed.
    pub fn finish_failed(&self, error: String) {
        let mut inner = self.inner.lock().expect("job lock poisoned");
        inner.state = JobState::Failed { error };
        inner.finished_at = Some(Instant::now());
    }

    /// Return true when the job is done.
    pub fn is_done(&self) -> bool {
        matches!(
            &self.inner.lock().expect("job lock poisoned").state,
            JobState::Done { .. }
        )
    }

    /// Return true when the job is finished and the retention time elapsed.
    fn is_expired(&self, retention: Duration) -> bool {
        let inner = self.inner.lock().expect("job lock poisoned");
        inner
            .finished_at
            .is_some_and(|finished_at| finished_at.elapsed() >= retention)
    }

    /// Ask the running task to stop.
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    /// Return a receiver that the task uses to observe a cancel request.
    pub fn cancel_receiver(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    /// Store the task handle of the job.
    pub fn set_task(&self, task: JoinHandle<()>) {
        *self.task.lock().expect("job task lock poisoned") = Some(task);
    }

    /// Wait for the job task when it is set.
    pub async fn wait_for_task(&self) {
        let task = self.task.lock().expect("job task lock poisoned").take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

struct JobStoreInner {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    retention: Duration,
}

/// The in-memory remote upload job store.
#[derive(Clone)]
pub struct JobStore {
    inner: Arc<JobStoreInner>,
}

impl Default for JobStore {
    fn default() -> Self {
        Self::new()
    }
}

impl JobStore {
    /// Create an empty job store with the default retention time.
    pub fn new() -> Self {
        Self::with_retention(JOB_RETENTION)
    }

    /// Create an empty job store with a retention time.
    pub fn with_retention(retention: Duration) -> Self {
        Self {
            inner: Arc::new(JobStoreInner {
                jobs: Mutex::new(HashMap::new()),
                retention,
            }),
        }
    }

    /// Add a new job and return its ID and handle.
    pub fn create(&self, path: PathBuf, total_bytes: Option<u64>) -> (String, Arc<Job>) {
        let mut jobs = self.inner.jobs.lock().expect("job store lock poisoned");
        let id = loop {
            let id = random_url_safe(JOB_ID_LENGTH);
            if !jobs.contains_key(&id) {
                break id;
            }
        };
        let job = Arc::new(Job::new(
            random_url_safe(JOB_TOKEN_LENGTH),
            path,
            total_bytes,
        ));
        jobs.insert(id.clone(), Arc::clone(&job));
        (id, job)
    }

    /// Return a live job. This method removes an expired job.
    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        let mut jobs = self.inner.jobs.lock().expect("job store lock poisoned");
        let job = jobs.get(id)?;
        if job.is_expired(self.inner.retention) {
            jobs.remove(id);
            return None;
        }
        Some(Arc::clone(job))
    }

    /// Remove a job and return its handle.
    pub fn remove(&self, id: &str) -> Option<Arc<Job>> {
        self.inner
            .jobs
            .lock()
            .expect("job store lock poisoned")
            .remove(id)
    }

    /// Remove every finished job that is past the retention time.
    /// Return the number of removed jobs.
    pub fn prune_expired(&self) -> usize {
        let mut jobs = self.inner.jobs.lock().expect("job store lock poisoned");
        let before = jobs.len();
        jobs.retain(|_, job| !job.is_expired(self.inner.retention));
        before - jobs.len()
    }
}

fn random_url_safe(length: usize) -> String {
    Alphanumeric
        .sample_iter(rand::rng())
        .take(length)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_job_with_a_random_id_and_token() {
        let store = JobStore::new();
        let (id, job) = store.create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), Some(1024));

        assert!(id.len() >= JOB_ID_LENGTH);
        assert!(job.token().len() >= JOB_TOKEN_LENGTH);
        assert!(store.get(&id).is_some());

        let status = job.status();
        assert!(matches!(status.state, JobState::Fetching));
        assert_eq!(status.received_bytes, 0);
        assert_eq!(status.total_bytes, Some(1024));
    }

    #[test]
    fn returns_the_job_status() {
        let store = JobStore::new();
        let (_id, job) = store.create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), Some(4096));

        job.mark_received(2048);
        let status = job.status();
        assert!(matches!(status.state, JobState::Fetching));
        assert_eq!(status.received_bytes, 2048);

        job.finish_done("http://localhost/file".to_owned());
        let status = job.status();
        assert_eq!(status.state.name(), "done");
        match status.state {
            JobState::Done { url } => assert_eq!(url, "http://localhost/file"),
            _ => panic!("job must be done"),
        }

        job.finish_failed("Fetch failed".to_owned());
        let status = job.status();
        assert_eq!(status.state.name(), "failed");
        match status.state {
            JobState::Failed { error } => assert_eq!(error, "Fetch failed"),
            _ => panic!("job must be failed"),
        }
    }

    #[test]
    fn sets_the_job_total_bytes() {
        let store = JobStore::new();
        let (_id, job) = store.create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None);

        assert_eq!(job.status().total_bytes, None);
        job.set_total_bytes(Some(2048));
        assert_eq!(job.status().total_bytes, Some(2048));
        job.set_total_bytes(None);
        assert_eq!(job.status().total_bytes, None);
    }

    #[test]
    fn checks_the_job_token() {
        let store = JobStore::new();
        let (_id, job) = store.create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None);
        let token = job.token().to_owned();

        assert!(job.token_matches(&token));
        assert!(!job.token_matches("not-the-token"));
        assert!(!job.token_matches(""));
    }

    #[test]
    fn removes_a_job() {
        let store = JobStore::new();
        let (id, _job) = store.create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None);

        assert!(store.remove(&id).is_some());
        assert!(store.get(&id).is_none());
        assert!(store.remove(&id).is_none());
    }

    #[test]
    fn expires_a_finished_job() {
        let store = JobStore::with_retention(Duration::ZERO);
        let (id, job) = store.create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None);

        assert!(store.get(&id).is_some());
        job.finish_done("http://localhost/file".to_owned());

        assert!(store.get(&id).is_none());
        assert_eq!(store.prune_expired(), 0);
    }

    #[test]
    fn prunes_finished_jobs() {
        let store = JobStore::with_retention(Duration::ZERO);
        let (done_id, done_job) = store.create(PathBuf::from("./a/ZmlsZQ"), None);
        let (fetching_id, _fetching_job) = store.create(PathBuf::from("./b/ZmlsZQ"), None);

        done_job.finish_failed("Fetch failed".to_owned());
        assert_eq!(store.prune_expired(), 1);
        assert!(store.get(&done_id).is_none());
        assert!(store.get(&fetching_id).is_some());
    }
}
