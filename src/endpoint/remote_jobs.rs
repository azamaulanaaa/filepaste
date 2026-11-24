use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::distr::Alphanumeric;
use rand::distr::Distribution;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The time that the store keeps a finished job.
pub const JOB_RETENTION: Duration = Duration::from_hours(1);

/// The error message for a job that a restart interrupted.
pub const RESTART_ERROR: &str = "Server restarted before the job completed";

/// The number of random characters in a job ID.
const JOB_ID_LENGTH: usize = 22;

/// The number of random characters in a job token.
const JOB_TOKEN_LENGTH: usize = 43;

/// The suffix of a persisted job record.
const RECORD_SUFFIX: &str = ".json";

/// The state of a remote upload job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
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

/// One persisted job record.
#[derive(Debug, Serialize, Deserialize)]
struct JobRecord {
    id: String,
    token: String,
    path: PathBuf,
    state: JobState,
    received_bytes: u64,
    total_bytes: Option<u64>,
    created_at: u64,
    updated_at: u64,
    finished_at: Option<u64>,
}

struct JobInner {
    state: JobState,
    received_bytes: u64,
    total_bytes: Option<u64>,
    created_at: u64,
    updated_at: u64,
    finished_at: Option<u64>,
    removed: bool,
}

/// One remote upload job.
pub struct Job {
    id: String,
    token: String,
    path: PathBuf,
    record_dir: Option<PathBuf>,
    cancel: watch::Sender<bool>,
    inner: Mutex<JobInner>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Job {
    fn new(
        id: String,
        token: String,
        path: PathBuf,
        total_bytes: Option<u64>,
        record_dir: Option<PathBuf>,
    ) -> Self {
        let now = unix_now();
        Self {
            id,
            token,
            path,
            record_dir,
            cancel: watch::channel(false).0,
            inner: Mutex::new(JobInner {
                state: JobState::Fetching,
                received_bytes: 0,
                total_bytes,
                created_at: now,
                updated_at: now,
                finished_at: None,
                removed: false,
            }),
            task: Mutex::new(None),
        }
    }

    fn from_record(record: JobRecord, record_dir: Option<PathBuf>) -> Self {
        Self {
            id: record.id,
            token: record.token,
            path: record.path,
            record_dir,
            cancel: watch::channel(false).0,
            inner: Mutex::new(JobInner {
                state: record.state,
                received_bytes: record.received_bytes,
                total_bytes: record.total_bytes,
                created_at: record.created_at,
                updated_at: record.updated_at,
                finished_at: record.finished_at,
                removed: false,
            }),
            task: Mutex::new(None),
        }
    }

    /// Return the job ID.
    pub fn id(&self) -> &str {
        &self.id
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
        JobStatus {
            state: inner.state.clone(),
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

    /// Mark the job as done and write the record.
    pub fn finish_done(&self, url: String) {
        self.finish(JobState::Done { url });
    }

    /// Mark the job as failed and write the record.
    pub fn finish_failed(&self, error: String) {
        self.finish(JobState::Failed { error });
    }

    fn finish(&self, state: JobState) {
        let mut inner = self.inner.lock().expect("job lock poisoned");
        if inner.removed {
            return;
        }
        inner.state = state;
        let now = unix_now();
        inner.updated_at = now;
        inner.finished_at = Some(now);
        let Some(dir) = &self.record_dir else {
            return;
        };
        if let Err(error) = write_record(dir, &self.record_from_inner(&inner)) {
            tracing::error!("Cannot write the record of job {}: {error}", self.id);
        }
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
        inner.finished_at.is_some_and(|finished_at| {
            unix_now().saturating_sub(finished_at) >= retention.as_secs()
        })
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

    fn record_from_inner(&self, inner: &JobInner) -> JobRecord {
        JobRecord {
            id: self.id.clone(),
            token: self.token.clone(),
            path: self.path.clone(),
            state: inner.state.clone(),
            received_bytes: inner.received_bytes,
            total_bytes: inner.total_bytes,
            created_at: inner.created_at,
            updated_at: inner.updated_at,
            finished_at: inner.finished_at,
        }
    }

    /// Write the record to the job store directory.
    fn persist(&self) -> io::Result<()> {
        let Some(dir) = &self.record_dir else {
            return Ok(());
        };
        let inner = self.inner.lock().expect("job lock poisoned");
        write_record(dir, &self.record_from_inner(&inner))
    }

    /// Stop the job from writing a further record.
    fn mark_removed(&self) {
        self.inner.lock().expect("job lock poisoned").removed = true;
    }
}

struct JobStoreInner {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    retention: Duration,
    record_dir: Option<PathBuf>,
}

/// The remote upload job store.
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
    /// Create an empty in-memory job store with the default retention time.
    pub fn new() -> Self {
        Self::with_retention(JOB_RETENTION)
    }

    /// Create an empty in-memory job store with a retention time.
    pub fn with_retention(retention: Duration) -> Self {
        Self {
            inner: Arc::new(JobStoreInner {
                jobs: Mutex::new(HashMap::new()),
                retention,
                record_dir: None,
            }),
        }
    }

    /// Open a durable job store with the default retention time.
    /// This method loads the records from `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with_retention(dir, JOB_RETENTION)
    }

    /// Open a durable job store with a retention time.
    /// This method loads the records from `dir`.
    pub fn open_with_retention(dir: impl Into<PathBuf>, retention: Duration) -> io::Result<Self> {
        let dir = dir.into();
        prepare_record_dir(&dir)?;
        let store = Self {
            inner: Arc::new(JobStoreInner {
                jobs: Mutex::new(HashMap::new()),
                retention,
                record_dir: Some(dir.clone()),
            }),
        };
        store.load_records(&dir)?;
        Ok(store)
    }

    /// Add a new job and return its ID and handle.
    /// The record is durable when this method returns `Ok`.
    pub fn create(
        &self,
        path: PathBuf,
        total_bytes: Option<u64>,
    ) -> io::Result<(String, Arc<Job>)> {
        let mut jobs = self.inner.jobs.lock().expect("job store lock poisoned");
        let id = loop {
            let id = random_url_safe(JOB_ID_LENGTH);
            if !jobs.contains_key(&id) {
                break id;
            }
        };
        let job = Arc::new(Job::new(
            id.clone(),
            random_url_safe(JOB_TOKEN_LENGTH),
            path,
            total_bytes,
            self.inner.record_dir.clone(),
        ));
        job.persist()?;
        jobs.insert(id.clone(), Arc::clone(&job));
        Ok((id, job))
    }

    /// Return a live job. This method removes an expired job.
    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        let mut jobs = self.inner.jobs.lock().expect("job store lock poisoned");
        let job = Arc::clone(jobs.get(id)?);
        if job.is_expired(self.inner.retention) {
            jobs.remove(id);
            job.mark_removed();
            drop(jobs);
            self.delete_record(id);
            return None;
        }
        Some(job)
    }

    /// Remove a job and return its handle.
    pub fn remove(&self, id: &str) -> Option<Arc<Job>> {
        let job = self
            .inner
            .jobs
            .lock()
            .expect("job store lock poisoned")
            .remove(id);
        if let Some(job) = &job {
            job.mark_removed();
            self.delete_record(id);
        }
        job
    }

    /// Remove every finished job that is past the retention time.
    /// Return the number of removed jobs.
    pub fn prune_expired(&self) -> usize {
        let mut jobs = self.inner.jobs.lock().expect("job store lock poisoned");
        let mut expired = Vec::new();
        jobs.retain(|id, job| {
            if job.is_expired(self.inner.retention) {
                expired.push((id.clone(), Arc::clone(job)));
                false
            } else {
                true
            }
        });
        drop(jobs);
        for (id, job) in &expired {
            job.mark_removed();
            self.delete_record(id);
        }
        expired.len()
    }

    /// Load the records from `dir`.
    /// A corrupt record is skipped with a warning.
    fn load_records(&self, dir: &Path) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!("Cannot read a job store entry: {error}");
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                tracing::warn!("Skipping the job store entry {:?}", path);
                continue;
            };
            let Some(id) = file_name.strip_suffix(RECORD_SUFFIX) else {
                continue;
            };
            if !valid_job_id(id) {
                tracing::warn!("Skipping the job store entry {:?}", path);
                continue;
            }
            let data = match fs::read(&path) {
                Ok(data) => data,
                Err(error) => {
                    tracing::warn!("Cannot read the job record {:?}: {error}", path);
                    continue;
                }
            };
            let mut record: JobRecord = match serde_json::from_slice(&data) {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!("Skipping the corrupt job record {:?}: {error}", path);
                    continue;
                }
            };
            if record.id != id {
                tracing::warn!("Skipping the job record {:?} with an unexpected ID", path);
                continue;
            }
            if record_expired(&record, self.inner.retention) {
                self.delete_record(id);
                continue;
            }
            if matches!(record.state, JobState::Fetching) {
                record.state = JobState::Failed {
                    error: RESTART_ERROR.to_owned(),
                };
                let now = unix_now();
                record.updated_at = now;
                record.finished_at = Some(now);
                if let Err(error) = write_record(dir, &record) {
                    tracing::warn!("Cannot write the job record {:?}: {error}", path);
                }
            }
            let job = Arc::new(Job::from_record(record, self.inner.record_dir.clone()));
            self.inner
                .jobs
                .lock()
                .expect("job store lock poisoned")
                .insert(id.to_owned(), job);
        }
        Ok(())
    }

    fn delete_record(&self, id: &str) {
        let Some(dir) = &self.inner.record_dir else {
            return;
        };
        if !valid_job_id(id) {
            return;
        }
        if let Err(error) = fs::remove_file(record_path(dir, id)) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!("Cannot delete the record of job {id}: {error}");
            }
        }
    }
}

/// Return true when the ID is safe to use in a record file name.
fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}{RECORD_SUFFIX}"))
}

fn record_expired(record: &JobRecord, retention: Duration) -> bool {
    record
        .finished_at
        .is_some_and(|finished_at| unix_now().saturating_sub(finished_at) >= retention.as_secs())
}

/// Write one record with a temporary file and a rename.
fn write_record(dir: &Path, record: &JobRecord) -> io::Result<()> {
    let data = serde_json::to_vec(record).map_err(io::Error::other)?;
    let target = record_path(dir, &record.id);
    let temp = dir.join(format!(".{}.{}.tmp", record.id, random_url_safe(12)));
    let result = (|| -> io::Result<()> {
        {
            let mut file = open_private_file(&temp)?;
            file.write_all(&data)?;
            file.sync_all()?;
        }
        fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn open_private_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().write(true).create_new(true).open(path)
    }
}

/// Create the record directory with owner-only access on Unix.
fn prepare_record_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
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
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), Some(1024))
            .expect("create a job");

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
        let (_id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), Some(4096))
            .expect("create a job");

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
        let (_id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");

        assert_eq!(job.status().total_bytes, None);
        job.set_total_bytes(Some(2048));
        assert_eq!(job.status().total_bytes, Some(2048));
        job.set_total_bytes(None);
        assert_eq!(job.status().total_bytes, None);
    }

    #[test]
    fn checks_the_job_token() {
        let store = JobStore::new();
        let (_id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");
        let token = job.token().to_owned();

        assert!(job.token_matches(&token));
        assert!(!job.token_matches("not-the-token"));
        assert!(!job.token_matches(""));
    }

    #[test]
    fn removes_a_job() {
        let store = JobStore::new();
        let (id, _job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");

        assert!(store.remove(&id).is_some());
        assert!(store.get(&id).is_none());
        assert!(store.remove(&id).is_none());
    }

    #[test]
    fn expires_a_finished_job() {
        let store = JobStore::with_retention(Duration::ZERO);
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");

        assert!(store.get(&id).is_some());
        job.finish_done("http://localhost/file".to_owned());

        assert!(store.get(&id).is_none());
        assert_eq!(store.prune_expired(), 0);
    }

    #[test]
    fn prunes_finished_jobs() {
        let store = JobStore::with_retention(Duration::ZERO);
        let (done_id, done_job) = store
            .create(PathBuf::from("./a/ZmlsZQ"), None)
            .expect("create the done job");
        let (fetching_id, _fetching_job) = store
            .create(PathBuf::from("./b/ZmlsZQ"), None)
            .expect("create the fetching job");

        done_job.finish_failed("Fetch failed".to_owned());
        assert_eq!(store.prune_expired(), 1);
        assert!(store.get(&done_id).is_none());
        assert!(store.get(&fetching_id).is_some());
    }

    #[test]
    fn reopens_a_done_job() {
        let dir = tempfile::tempdir().expect("create the record directory");
        let path = PathBuf::from("./AbCdEfGh/ZmlsZQ");
        let (id, token) = {
            let store = JobStore::open(dir.path()).expect("open the job store");
            let (id, job) = store
                .create(path.clone(), Some(1024))
                .expect("create a job");
            job.mark_received(1024);
            job.finish_done("http://localhost/file".to_owned());
            (id, job.token().to_owned())
        };

        let store = JobStore::open(dir.path()).expect("reopen the job store");
        let job = store.get(&id).expect("the job must survive a reopen");

        assert!(job.token_matches(&token));
        assert_eq!(job.path(), path.as_path());
        let status = job.status();
        assert_eq!(status.received_bytes, 1024);
        assert_eq!(status.total_bytes, Some(1024));
        match status.state {
            JobState::Done { url } => assert_eq!(url, "http://localhost/file"),
            _ => panic!("job must be done"),
        }
    }

    #[test]
    fn reopens_a_fetching_job_as_failed() {
        let dir = tempfile::tempdir().expect("create the record directory");
        let id = {
            let store = JobStore::open(dir.path()).expect("open the job store");
            let (id, _job) = store
                .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
                .expect("create a job");
            id
        };

        let store = JobStore::open(dir.path()).expect("reopen the job store");
        let job = store.get(&id).expect("the job must survive a reopen");

        match job.status().state {
            JobState::Failed { error } => assert_eq!(error, RESTART_ERROR),
            _ => panic!("job must be failed"),
        }
    }

    #[test]
    fn removes_an_expired_record_at_startup() {
        let dir = tempfile::tempdir().expect("create the record directory");
        let id = {
            let store =
                JobStore::open_with_retention(dir.path(), Duration::ZERO).expect("open the store");
            let (id, job) = store
                .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
                .expect("create a job");
            job.finish_failed("Fetch failed".to_owned());
            id
        };

        let store =
            JobStore::open_with_retention(dir.path(), Duration::ZERO).expect("reopen the store");

        assert!(store.get(&id).is_none());
        assert!(!record_path(dir.path(), &id).exists());
    }

    #[test]
    fn skips_corrupt_records() {
        let dir = tempfile::tempdir().expect("create the record directory");
        fs::write(dir.path().join("broken.json"), b"{\"id\":").expect("write the broken record");
        fs::write(dir.path().join("partial.json"), b"{\"id\":\"partial\"}")
            .expect("write the partial record");

        let store = JobStore::open(dir.path()).expect("open the job store");
        let (id, _job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");

        assert!(store.get(&id).is_some());
    }

    #[test]
    fn removes_the_record_of_a_removed_job() {
        let dir = tempfile::tempdir().expect("create the record directory");
        let store = JobStore::open(dir.path()).expect("open the job store");
        let (id, _job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");
        let path = record_path(dir.path(), &id);
        assert!(path.exists());

        assert!(store.remove(&id).is_some());

        assert!(!path.exists());
    }

    #[test]
    fn does_not_write_a_record_after_a_remove() {
        let dir = tempfile::tempdir().expect("create the record directory");
        let store = JobStore::open(dir.path()).expect("open the job store");
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");
        let path = record_path(dir.path(), &id);
        assert!(store.remove(&id).is_some());

        job.finish_failed("Fetch failed".to_owned());

        assert!(!path.exists());
    }

    #[test]
    fn reports_a_record_that_cannot_be_written() {
        let dir = tempfile::tempdir().expect("create the record directory");
        let store = JobStore::open(dir.path()).expect("open the job store");
        fs::remove_dir_all(dir.path()).expect("remove the record directory");

        let result = store.create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None);

        assert!(result.is_err());
        assert_eq!(store.prune_expired(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn restricts_the_record_directory_and_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create the record directory");
        let store = JobStore::open(dir.path()).expect("open the job store");
        let directory_mode = fs::metadata(dir.path())
            .expect("read the record directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);

        let (id, _job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");
        let file_mode = fs::metadata(record_path(dir.path(), &id))
            .expect("read the job record")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }
}
