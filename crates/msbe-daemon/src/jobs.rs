//! The daemon's job queue (`docs/03-architecture.md` §3.4).
//!
//! Jobs run one at a time on a worker thread. While one runs it holds the instance-state lock,
//! so the listener answers state requests as busy but still reports progress and accepts
//! cancellation. Cancellation is cooperative: operations check it between steps and before they
//! commit, so a cancelled job leaves no partial profile or output.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
};

use msbe_core::instance::Name;
use msbe_pack::{PackError, Progress};
use msbe_rpc_schema::{JobEvent, JobEventRecord, JobState, JobStatus};
use serde_json::Value;

use crate::{
    Connector, Downloads,
    pack::{self, Plan},
};

/// What a job does.
#[derive(Debug)]
pub(crate) enum Work {
    /// Runs a held preview.
    Plan(Box<Plan>),
    /// Writes an instance snapshot.
    SnapshotCreate { instance: Name, output: PathBuf },
    /// Restores an instance snapshot.
    SnapshotRestore { input: PathBuf },
    /// Adds a download queue item's files to its profile: the queue's instance lane.
    DownloadAdd { id: u64 },
    /// Updates a profile's mods from their providers.
    UpdateApply(msbe_rpc_schema::ProfileRequest),
}

/// What a job runs against.
pub(crate) struct Environment {
    pub(crate) home: Option<PathBuf>,
    pub(crate) connect: Connector,
    pub(crate) downloads: Arc<Downloads>,
}

impl fmt::Debug for Environment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Environment")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Job {
    method: String,
    state: JobState,
    events: Vec<JobEventRecord>,
    next_sequence: u64,
    cancel: Arc<AtomicBool>,
    pending: Option<(Work, Environment)>,
}

#[derive(Debug, Default)]
struct Table {
    next: u64,
    queue: VecDeque<u64>,
    jobs: BTreeMap<u64, Job>,
}

/// A queued job taken by the worker: its ID, work, environment and cancellation flag.
type Claimed = (u64, Work, Environment, Arc<AtomicBool>);

/// A job holds the instance state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Busy;

/// Long operations, queued and run one at a time.
#[derive(Debug, Default)]
pub struct Jobs {
    table: Mutex<Table>,
    ready: Condvar,
    state: Mutex<()>,
}

impl Jobs {
    /// Queues `work` and returns its job ID.
    pub(crate) fn submit(&self, method: &str, work: Work, environment: Environment) -> u64 {
        let mut table = self.table();
        table.next += 1;
        let id = table.next;
        table.jobs.insert(
            id,
            Job {
                method: method.to_owned(),
                state: JobState::Queued,
                events: Vec::new(),
                next_sequence: 1,
                cancel: Arc::new(AtomicBool::new(false)),
                pending: Some((work, environment)),
            },
        );
        table.queue.push_back(id);
        drop(table);
        self.ready.notify_one();
        id
    }

    /// The instance-state lock, unless a job holds it.
    pub(crate) fn try_state(&self) -> Result<MutexGuard<'_, ()>, Busy> {
        match self.state.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => Err(Busy),
        }
    }

    /// The instance-state lock, once a running job releases it.
    pub(crate) fn lock_state(&self) -> MutexGuard<'_, ()> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Job `id`'s state and its events after sequence `after`.
    pub(crate) fn status(&self, id: u64, after: u64) -> Option<JobStatus> {
        let table = self.table();
        let job = table.jobs.get(&id)?;
        let events: Vec<JobEventRecord> = job
            .events
            .iter()
            .filter(|record| record.sequence > after)
            .cloned()
            .collect();
        Some(JobStatus {
            job_id: id,
            method: job.method.clone(),
            state: job.state,
            next: events.last().map_or(after, |record| record.sequence),
            events,
        })
    }

    /// Asks job `id` to stop. A queued job is cancelled at once; a running one stops at its next
    /// checkpoint. Returns whether the request can still take effect.
    pub(crate) fn cancel(&self, id: u64) -> Option<bool> {
        let mut table = self.table();
        let job = table.jobs.get_mut(&id)?;
        Some(match job.state {
            JobState::Queued => {
                job.pending = None;
                job.state = JobState::Cancelled;
                record(job, JobEvent::Cancelled);
                true
            }
            JobState::Running => {
                job.cancel.store(true, Ordering::SeqCst);
                true
            }
            JobState::Succeeded | JobState::Failed | JobState::Cancelled => false,
        })
    }

    /// Runs the oldest queued job to completion, returning whether there was one.
    pub fn run_next(&self) -> bool {
        let Some((id, work, environment, cancel)) = self.claim() else {
            return false;
        };
        let _state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let progress = JobProgress {
            jobs: self,
            id,
            cancel: &cancel,
        };
        let outcome = pack::execute(work, &environment, &progress);
        let (state, event) = match outcome {
            Ok(result) => (JobState::Succeeded, JobEvent::Done { result }),
            Err(PackError::Cancelled) => (JobState::Cancelled, JobEvent::Cancelled),
            Err(error) => (
                JobState::Failed,
                JobEvent::Failed {
                    code: format!("{:?}", error.code()),
                    message: error.to_string(),
                    issues: serde_json::to_value(error.issues()).unwrap_or(Value::Null),
                },
            ),
        };
        if let Some(job) = self.table().jobs.get_mut(&id) {
            record(job, event);
            job.state = state;
        }
        true
    }

    /// Blocks until a job is queued, then runs it.
    pub fn wait_and_run(&self) {
        let mut table = self.table();
        while table.queue.is_empty() {
            table = self
                .ready
                .wait(table)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(table);
        self.run_next();
    }

    fn claim(&self) -> Option<Claimed> {
        let mut table = self.table();
        while let Some(id) = table.queue.pop_front() {
            if let Some(job) = table.jobs.get_mut(&id)
                && let Some((work, environment)) = job.pending.take()
            {
                job.state = JobState::Running;
                return Some((id, work, environment, Arc::clone(&job.cancel)));
            }
        }
        None
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Appends `event`, replacing a trailing progress event with a newer one.
fn record(job: &mut Job, event: JobEvent) {
    let replaces = matches!(event, JobEvent::Progress { .. })
        && matches!(
            job.events.last(),
            Some(JobEventRecord {
                event: JobEvent::Progress { .. },
                ..
            })
        );
    if replaces {
        job.events.pop();
    }
    job.events.push(JobEventRecord {
        sequence: job.next_sequence,
        event,
    });
    job.next_sequence += 1;
}

/// Progress for one running job.
struct JobProgress<'a> {
    jobs: &'a Jobs,
    id: u64,
    cancel: &'a AtomicBool,
}

impl Progress for JobProgress<'_> {
    fn report(&self, completed: u64, total: u64, message: &str) {
        if let Some(job) = self.jobs.table().jobs.get_mut(&self.id) {
            record(
                job,
                JobEvent::Progress {
                    completed,
                    total,
                    message: message.to_owned(),
                },
            );
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}
