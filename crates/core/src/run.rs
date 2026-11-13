//! A `Run` is one working session on one project: you can talk to the agents,
//! plan, build, review, apply, and then start the next round, all in one run.
//!
//! Several agents work on a run at the same time, so its changing parts sit
//! behind a `Mutex`: whoever wants to read or change them locks it first, and
//! the lock is released when the guard goes out of scope. Locks here are only
//! held for a few lines and never across an `.await`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::oneshot;

use crate::config::{Review, Task};
use crate::event::{AgentStatus, Event, EventKind};
use crate::worktrees::Workspace;

/// The stages a run moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Nothing running and no changes waiting: talk, brainstorm or plan.
    Ready,
    /// The user sent a message and an agent is replying.
    Chatting,
    Planning,
    AwaitingApproval,
    Building,
    Reviewing,
    /// The review is in: apply the changes, discard them, or send work back.
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
    /// The first message of the run.
    pub request: String,
    sink: EventSink,
    inner: Mutex<Inner>,
}

/// One plan being built: its tasks, the workers' reports and the review. Opened
/// when the plan is approved, closed by Apply or Discard.
#[derive(Default)]
pub(crate) struct Round {
    /// The git worktrees, or `None` when the agents work directly in the project
    /// folder (tests, and the planner-only example).
    pub(crate) workspace: Option<Arc<Workspace>>,
    pub(crate) worker_reports: HashMap<usize, String>,
    pub(crate) review: Option<Review>,
    /// The commit of the review worktree at the last review, so a second review
    /// only needs the changes made since.
    pub(crate) last_reviewed: Option<String>,
}

/// The parts of a run that change while it is in progress.
struct Inner {
    state: RunState,
    events: Vec<Event>,
    /// The latest plan's tasks (the approved ones once a round is open).
    tasks: Vec<Task>,
    running_agents: BTreeSet<String>,
    /// Agent ID -> its latest session ID, so the agent can be resumed and
    /// remembers the conversation (for a fix, or when the user talks to it).
    sessions: HashMap<String, String>,
    round: Option<Round>,
    rounds_started: usize,
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
                state: RunState::Ready,
                events: Vec::new(),
                tasks: Vec::new(),
                running_agents: BTreeSet::new(),
                sessions: HashMap::new(),
                round: None,
                rounds_started: 0,
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

    // --- Plan and sessions ----------------------------------------------------------

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

    /// The latest session of an agent, if it has finished one successfully.
    pub fn session(&self, agent: &str) -> Option<String> {
        self.lock().sessions.get(agent).cloned()
    }

    pub(crate) fn record_session(&self, agent: &str, session: String) {
        self.lock().sessions.insert(agent.to_string(), session);
    }

    // --- The open round ----------------------------------------------------------

    /// True while a round's changes are waiting to be applied or discarded.
    pub fn has_open_round(&self) -> bool {
        self.lock().round.is_some()
    }

    /// Start a round; returns its number (1 for the first round of this run).
    pub(crate) fn open_round(&self, workspace: Option<Arc<Workspace>>) -> usize {
        let number = {
            let mut inner = self.lock();
            inner.rounds_started += 1;
            inner.round = Some(Round { workspace, ..Round::default() });
            inner.rounds_started
        };
        self.emit(None, EventKind::Round { open: true });
        number
    }

    /// The number the next round will get.
    pub(crate) fn next_round_number(&self) -> usize {
        self.lock().rounds_started + 1
    }

    /// End the round. The workers' and reviewer's sessions end with it: they lived in
    /// worktrees that are now gone, so they cannot be resumed.
    pub(crate) fn close_round(&self) -> Option<Arc<Workspace>> {
        let workspace = {
            let mut inner = self.lock();
            inner.sessions.retain(|agent, _| !agent.starts_with("worker-") && agent != "reviewer");
            inner.round.take().and_then(|round| round.workspace)
        };
        self.emit(None, EventKind::Round { open: false });
        workspace
    }

    pub(crate) fn workspace(&self) -> Option<Arc<Workspace>> {
        self.lock().round.as_ref().and_then(|round| round.workspace.clone())
    }

    /// The folder an agent works in: its worktree during a round, else the project.
    pub fn folder_for(&self, agent: &str) -> PathBuf {
        let workspace = self.workspace();
        match (workspace, crate::config::worker_number(agent)) {
            (Some(workspace), Some(number)) => workspace.worker_dir(number),
            (Some(workspace), None) if agent == crate::config::REVIEWER_ID => workspace.review_dir(),
            _ => self.project_dir.clone(),
        }
    }

    pub fn project(&self) -> &Path {
        &self.project_dir
    }

    pub fn worker_report(&self, number: usize) -> Option<String> {
        self.lock().round.as_ref().and_then(|round| round.worker_reports.get(&number).cloned())
    }

    pub(crate) fn record_worker(&self, number: usize, report: String) {
        if let Some(round) = self.lock().round.as_mut() {
            round.worker_reports.insert(number, report);
        }
    }

    /// Numbers of the workers that finished their task in this round.
    pub(crate) fn finished_workers(&self) -> Vec<usize> {
        let inner = self.lock();
        let mut numbers: Vec<usize> = inner
            .round
            .as_ref()
            .map(|round| round.worker_reports.keys().copied().collect())
            .unwrap_or_default();
        numbers.sort_unstable();
        numbers
    }

    pub fn review(&self) -> Option<Review> {
        self.lock().round.as_ref().and_then(|round| round.review.clone())
    }

    pub(crate) fn record_review(&self, review: Review, reviewed_commit: Option<String>) {
        if let Some(round) = self.lock().round.as_mut() {
            round.review = Some(review);
            if reviewed_commit.is_some() {
                round.last_reviewed = reviewed_commit;
            }
        }
    }

    pub(crate) fn last_reviewed(&self) -> Option<String> {
        self.lock().round.as_ref().and_then(|round| round.last_reviewed.clone())
    }

    // --- Stopping ---------------------------------------------------------------

    /// True when nothing is running and the user may start something.
    pub fn is_at_rest(&self) -> bool {
        let at_rest = matches!(
            self.state(),
            RunState::Ready
                | RunState::AwaitingApproval
                | RunState::Done
                | RunState::Stopped
                | RunState::Failed
        );
        at_rest && !self.is_busy()
    }

    /// True while a phase (chatting, planning, building, reviewing) is running.
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
