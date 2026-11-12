//! A `Run` is one job, from "Start planning" to the final reviews.
//!
//! Several agents work on a run at the same time, so its changing parts sit
//! behind a `Mutex`: whoever wants to read or change them locks it first, and
//! the lock is released when the guard goes out of scope. Locks here are only
//! held for a few lines and never across an `.await`.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::oneshot;

use crate::config::{Review, Task};
use crate::event::{AgentStatus, Event, EventKind};

/// The stages a run moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Planning,
    AwaitingApproval,
    Building,
    Reviewing,
    Done,
    Failed,
    Stopped,
}

/// Called with the run's ID and every new event. The app uses it to forward
/// events to the window.
pub type EventSink = Arc<dyn Fn(&str, &Event) + Send + Sync>;

pub struct Run {
    pub id: String,
    pub project_dir: PathBuf,
    pub request: String,
    sink: EventSink,
    inner: Mutex<Inner>,
}

/// The parts of a run that change while it is in progress.
struct Inner {
    state: RunState,
    events: Vec<Event>,
    tasks: Vec<Task>,
    running_agents: BTreeSet<String>,
    /// Worker number -> session ID, so a worker can be resumed for a fix.
    worker_sessions: HashMap<usize, String>,
    worker_reports: HashMap<usize, String>,
    reviews: HashMap<usize, Review>,
    /// Present while a phase is running; sending on it stops that phase.
    stop_signal: Option<oneshot::Sender<()>>,
}

impl Run {
    pub fn new(project_dir: impl Into<PathBuf>, request: impl Into<String>, sink: EventSink) -> Arc<Run> {
        Arc::new(Run {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            project_dir: project_dir.into(),
            request: request.into(),
            sink,
            inner: Mutex::new(Inner {
                state: RunState::Planning,
                events: Vec::new(),
                tasks: Vec::new(),
                running_agents: BTreeSet::new(),
                worker_sessions: HashMap::new(),
                worker_reports: HashMap::new(),
                reviews: HashMap::new(),
                stop_signal: None,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned mutex means another thread panicked while holding it. The
        // data is still usable for us, so carry on rather than panic as well.
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // --- Events ---------------------------------------------------------------

    /// Record one event and pass it to the sink.
    pub fn emit(&self, agent: Option<&str>, kind: EventKind) {
        let mut inner = self.lock();
        let event = Event { seq: inner.events.len(), agent: agent.map(str::to_string), kind };
        inner.events.push(event.clone());
        // Called while still holding the lock so events reach the sink in order.
        (self.sink)(&self.id, &event);
    }

    /// Everything that has happened so far, for a window that (re)connects.
    pub fn events(&self) -> Vec<Event> {
        self.lock().events.clone()
    }

    // --- State ----------------------------------------------------------------

    pub fn state(&self) -> RunState {
        self.lock().state
    }

    pub fn set_state(&self, state: RunState) {
        self.lock().state = state;
        self.emit(None, EventKind::RunState { state });
    }

    pub(crate) fn agent_started(&self, agent: &str) {
        self.lock().running_agents.insert(agent.to_string());
        self.emit(Some(agent), EventKind::AgentStatus { status: AgentStatus::Running });
    }

    pub(crate) fn agent_finished(&self, agent: &str, status: AgentStatus) {
        self.lock().running_agents.remove(agent);
        self.emit(Some(agent), EventKind::AgentStatus { status });
    }

    // --- Plan, reports and reviews --------------------------------------------

    pub fn tasks(&self) -> Vec<Task> {
        self.lock().tasks.clone()
    }

    pub fn set_tasks(&self, tasks: Vec<Task>) {
        self.lock().tasks = tasks;
    }

    /// The task for worker `number` (counting from 1).
    pub fn task(&self, number: usize) -> Option<Task> {
        self.lock().tasks.get(number.wrapping_sub(1)).cloned()
    }

    pub fn worker_session(&self, number: usize) -> Option<String> {
        self.lock().worker_sessions.get(&number).cloned()
    }

    pub fn worker_report(&self, number: usize) -> Option<String> {
        self.lock().worker_reports.get(&number).cloned()
    }

    pub(crate) fn record_worker(&self, number: usize, report: String, session: Option<String>) {
        let mut inner = self.lock();
        inner.worker_reports.insert(number, report);
        if let Some(session) = session {
            inner.worker_sessions.insert(number, session);
        }
    }

    pub fn review(&self, number: usize) -> Option<Review> {
        self.lock().reviews.get(&number).cloned()
    }

    pub(crate) fn record_review(&self, number: usize, review: Review) {
        self.lock().reviews.insert(number, review);
    }

    // --- Stopping ---------------------------------------------------------------

    /// True while a phase (planning, building, reviewing) is running.
    pub fn is_busy(&self) -> bool {
        self.lock().stop_signal.is_some()
    }

    /// Ask the running phase to stop. Returns false if nothing was running.
    pub fn stop(&self) -> bool {
        match self.lock().stop_signal.take() {
            Some(signal) => signal.send(()).is_ok(),
            None => false,
        }
    }

    pub(crate) fn begin_phase(&self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.lock().stop_signal = Some(sender);
        receiver
    }

    pub(crate) fn end_phase(&self) {
        self.lock().stop_signal = None;
    }
}
