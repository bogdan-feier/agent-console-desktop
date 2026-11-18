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

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::config::{Review, Task};
use crate::event::{AgentStatus, Event, EventKind};
use crate::worktrees::Workspace;

/// Nothing is running after a restart, so a state that means "an agent is busy"
/// becomes Stopped: the round, if there is one, is still there to deal with.
fn at_rest(state: RunState) -> RunState {
    match state {
        RunState::Chatting | RunState::Planning | RunState::Building | RunState::Reviewing => {
            RunState::Stopped
        }
        settled => settled,
    }
}

/// The stages a run moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// True for a run read back from disk. Its agents' sessions and worktrees are
    /// gone with the app that made them, so it can be read but not continued.
    pub restored: bool,
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
    /// Opened by a quick change: there is no plan, so the one worker was never told
    /// which files it may touch and has to decide for itself.
    pub(crate) quick: bool,
}

/// What has to be kept to pick a run up again after the app has been closed.
///
/// The events in the run's file say what happened; this says where the run got
/// to: which agent sessions to resume, what the round is, and what was planned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: RunState,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub plan_summary: String,
    /// Agent ID -> the session `claude --resume` continues.
    pub sessions: HashMap<String, String>,
    pub rounds_started: usize,
    pub round: Option<RoundSnapshot>,
}

/// The open round, if there is one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSnapshot {
    pub worker_reports: HashMap<usize, String>,
    pub review: Option<Review>,
    pub last_reviewed: Option<String>,
    /// `#[serde(default)]` so rounds saved before quick changes existed still load.
    #[serde(default)]
    pub quick: bool,
    /// Enough to find the round's worktrees again: the rest comes from the run
    /// itself (the project folder, the run's ID, the round number).
    pub workspace: Option<WorkspaceSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub base: String,
    pub workers: usize,
}

/// The parts of a run that change while it is in progress.
struct Inner {
    state: RunState,
    events: Vec<Event>,
    /// The latest plan's tasks (the approved ones once a round is open).
    tasks: Vec<Task>,
    /// What the planner said the plan was for, used as a suggested commit message.
    plan_summary: String,
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
            restored: false,
            sink,
            inner: Mutex::new(Inner {
                state: RunState::Ready,
                events: Vec::new(),
                tasks: Vec::new(),
                plan_summary: String::new(),
                running_agents: BTreeSet::new(),
                sessions: HashMap::new(),
                round: None,
                rounds_started: 0,
                stop_signal: None,
            }),
        })
    }

    /// Where the run has got to, for saving to disk.
    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            state: inner.state,
            tasks: inner.tasks.clone(),
            plan_summary: inner.plan_summary.clone(),
            sessions: inner.sessions.clone(),
            rounds_started: inner.rounds_started,
            round: inner.round.as_ref().map(|round| RoundSnapshot {
                quick: round.quick,
                worker_reports: round.worker_reports.clone(),
                review: round.review.clone(),
                last_reviewed: round.last_reviewed.clone(),
                workspace: round.workspace.as_ref().map(|workspace| WorkspaceSnapshot {
                    base: workspace.base().to_string(),
                    workers: workspace.worker_count(),
                }),
            }),
        }
    }

    /// Rebuild a run from what was saved on disk.
    ///
    /// With no snapshot, or with one whose round could not be opened again, the
    /// run is marked `restored`: it can be read but not continued, because its
    /// agent sessions or its worktrees are gone. Otherwise it carries on where
    /// it left off.
    ///
    /// A run that was in the middle of something when the app closed comes back
    /// as `Stopped`: whatever was running died with the app, but an open round
    /// is still there to apply, discard or review again.
    pub fn from_saved(
        id: String,
        project_dir: impl Into<PathBuf>,
        request: impl Into<String>,
        events: Vec<Event>,
        snapshot: Option<Snapshot>,
        workspace: Option<Arc<Workspace>>,
        sink: EventSink,
    ) -> Arc<Run> {
        let last_state = events
            .iter()
            .rev()
            .find_map(|event| match event.kind {
                EventKind::RunState { state } => Some(state),
                _ => None,
            })
            .unwrap_or(RunState::Ready);
        let restored = snapshot.is_none();
        let saved = snapshot.unwrap_or(Snapshot {
            state: last_state,
            tasks: Vec::new(),
            plan_summary: String::new(),
            sessions: HashMap::new(),
            rounds_started: 0,
            round: None,
        });
        let round = saved.round.map(|round| Round {
            workspace,
            worker_reports: round.worker_reports,
            review: round.review,
            last_reviewed: round.last_reviewed,
            quick: round.quick,
        });
        let run = Arc::new(Run {
            id,
            project_dir: project_dir.into(),
            request: request.into(),
            restored,
            sink,
            inner: Mutex::new(Inner {
                state: at_rest(saved.state),
                events,
                tasks: saved.tasks,
                plan_summary: saved.plan_summary,
                running_agents: BTreeSet::new(),
                sessions: saved.sessions,
                round,
                rounds_started: saved.rounds_started,
                stop_signal: None,
            }),
        });
        run.close_what_was_interrupted();
        run
    }

    /// Bring the run's events in line with what is true after a restart.
    ///
    /// The log stops wherever the app did. An agent it last showed as running is no
    /// longer running, and a round the log shows open was not kept if the app closed
    /// before saving it. Left alone, the window would replay the log and show a
    /// worker working on nothing, with no way to stop it. So say how things stand.
    fn close_what_was_interrupted(&self) {
        let (still_running, logged_state, logged_round_open, state, round_open) = {
            let inner = self.lock();
            let mut last_status: HashMap<&str, AgentStatus> = HashMap::new();
            let (mut logged_state, mut logged_round_open) = (None, false);
            for event in &inner.events {
                match (&event.agent, &event.kind) {
                    (Some(agent), EventKind::AgentStatus { status }) => {
                        last_status.insert(agent, *status);
                    }
                    (_, EventKind::RunState { state }) => logged_state = Some(*state),
                    (_, EventKind::Round { open, .. }) => logged_round_open = *open,
                    _ => {}
                }
            }
            let mut running: Vec<String> = last_status
                .into_iter()
                .filter(|(_, status)| *status == AgentStatus::Running)
                .map(|(agent, _)| agent.to_string())
                .collect();
            running.sort();
            (running, logged_state, logged_round_open, inner.state, inner.round.is_some())
        };
        for agent in &still_running {
            self.emit(Some(agent), EventKind::AgentStatus { status: AgentStatus::Stopped });
        }
        if logged_round_open && !round_open {
            let text = "The app closed before this round was saved, so its work was lost. \
                        Approve the plan again to start it over."
                .to_string();
            self.emit(None, EventKind::Notice { text });
            self.emit(None, EventKind::Round { open: false, quick: false, tasks: Vec::new() });
        }
        if logged_state.is_some_and(|logged| logged != state) {
            self.emit(None, EventKind::RunState { state });
        }
    }

    /// True when this run can still be worked on.
    pub fn resumable(&self) -> bool {
        !self.restored
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

    /// What the plan was for, in the planner's words.
    pub fn plan_summary(&self) -> String {
        self.lock().plan_summary.clone()
    }

    pub(crate) fn set_plan_summary(&self, summary: String) {
        self.lock().plan_summary = summary;
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

    /// Forget an agent's session, so its next message starts a conversation from
    /// nothing. The way out of a session that has grown long and expensive.
    pub fn forget_session(&self, agent: &str) -> bool {
        self.lock().sessions.remove(agent).is_some()
    }

    // --- The open round ----------------------------------------------------------

    /// True while a round's changes are waiting to be applied or discarded.
    pub fn has_open_round(&self) -> bool {
        self.lock().round.is_some()
    }

    /// Start a round; returns its number (1 for the first round of this run).
    pub(crate) fn open_round(&self, workspace: Option<Arc<Workspace>>, quick: bool) -> usize {
        let number = {
            let mut inner = self.lock();
            inner.rounds_started += 1;
            inner.round = Some(Round { workspace, quick, ..Round::default() });
            inner.rounds_started
        };
        let tasks = self.tasks().into_iter().map(|task| task.title).collect();
        self.emit(None, EventKind::Round { open: true, quick, tasks });
        number
    }

    /// The number the next round will get.
    /// How many rounds this run has opened so far.
    /// True while a quick change's round is open: no plan, so no file list.
    pub(crate) fn round_is_quick(&self) -> bool {
        self.lock().round.as_ref().is_some_and(|round| round.quick)
    }

    pub(crate) fn rounds_started(&self) -> usize {
        self.lock().rounds_started
    }

    pub(crate) fn next_round_number(&self) -> usize {
        self.lock().rounds_started + 1
    }

    /// End the round. The workers' and reviewer's sessions end with it: they lived in
    /// worktrees that are now gone, so they cannot be resumed.
    pub(crate) fn close_round(&self) -> Option<Arc<Workspace>> {
        let workspace = {
            let mut inner = self.lock();
            inner.sessions.retain(|agent, _| {
                !agent.starts_with("worker-") && agent != "reviewer" && agent != "resolver"
            });
            inner.round.take().and_then(|round| round.workspace)
        };
        self.emit(None, EventKind::Round { open: false, quick: false, tasks: Vec::new() });
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
            // The reviewer and the resolver both work on everything merged together.
            (Some(workspace), None)
                if agent == crate::config::REVIEWER_ID || agent == crate::config::RESOLVER_ID =>
            {
                workspace.review_dir()
            }
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

    /// The commit message to offer for the open round, from what belongs to this round:
    /// its review, then its plan (a quick round has none, and the summary left over is
    /// an earlier round's), then its own task. The run's first message comes last,
    /// because a run often opens with a question rather than the change being applied.
    pub fn suggested_commit_message(&self) -> String {
        let inner = self.lock();
        let round = inner.round.as_ref();
        let reviewed =
            round.and_then(|round| round.review.as_ref()).map(|review| review.commit_message.clone());
        let planned = round.filter(|round| !round.quick).map(|_| inner.plan_summary.clone());
        let task = inner.tasks.first().map(|task| task.instructions.clone());
        [reviewed, planned, task, Some(self.request.clone())]
            .into_iter()
            .flatten()
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default()
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

    /// A phase ended without finishing: something inside the app panicked. The
    /// run is left able to act again — without this it would wait for a phase
    /// that will never end, with Stop unable to help because nothing is listening.
    pub fn phase_crashed(&self, message: &str) {
        self.end_phase();
        self.emit(None, EventKind::Error { message: message.to_string(), tidy_with: None });
        self.set_state(RunState::Failed);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn task(instructions: &str) -> Task {
        Task {
            title: instructions.to_string(),
            instructions: instructions.to_string(),
            files: Vec::new(),
            context: String::new(),
            hard: false,
        }
    }

    #[test]
    fn a_run_reopened_after_the_app_closed_mid_build_shows_nothing_still_running() {
        // The log as it was when the app was quit: a round opened, a worker started.
        let logged = [
            (None, EventKind::RunState { state: RunState::AwaitingApproval }),
            (None, EventKind::Round { open: true, quick: false, tasks: vec!["Add a button".into()] }),
            (None, EventKind::RunState { state: RunState::Building }),
            (Some("worker-1"), EventKind::AgentStatus { status: AgentStatus::Running }),
            (Some("planner"), EventKind::AgentStatus { status: AgentStatus::Done }),
        ];
        let events = logged
            .into_iter()
            .enumerate()
            .map(|(seq, (agent, kind))| Event { seq, agent: agent.map(str::to_string), kind })
            .collect();
        // What was saved last: the plan waiting for approval, before the round opened.
        let saved = Snapshot {
            state: RunState::AwaitingApproval,
            tasks: vec![task("Add a button")],
            plan_summary: String::new(),
            sessions: HashMap::new(),
            rounds_started: 8,
            round: None,
        };
        let run = Run::from_saved(
            "r".into(),
            "/p",
            "req",
            events,
            Some(saved),
            None,
            Arc::new(|_: &str, _: &Event| {}),
        );

        let added: Vec<EventKind> = run.events().into_iter().skip(5).map(|event| event.kind).collect();
        assert!(matches!(added[0], EventKind::AgentStatus { status: AgentStatus::Stopped }), "{added:?}");
        assert!(added.iter().any(|kind| matches!(kind, EventKind::Round { open: false, .. })));
        assert!(matches!(added.last(), Some(EventKind::RunState { state: RunState::AwaitingApproval })));
        assert_eq!(run.events()[5].agent.as_deref(), Some("worker-1"), "only the agent that was running");
        assert_eq!(run.state(), RunState::AwaitingApproval, "the plan can be approved again");

        // Reopened a second time, there is nothing left to correct.
        let again = Run::from_saved(
            "r".into(),
            "/p",
            "req",
            run.events(),
            Some(run.snapshot()),
            None,
            Arc::new(|_: &str, _: &Event| {}),
        );
        assert_eq!(again.events().len(), run.events().len());
    }

    #[test]
    fn a_quick_round_is_named_after_its_own_change() {
        let run = Run::new("/p", "Why are overdue reminders never sent?", Arc::new(|_: &str, _: &Event| {}));
        // An earlier planned round left its summary behind.
        run.set_plan_summary("Add remove-car".to_string());

        run.set_tasks(vec![task("Print No vehicles yet when the list is empty")]);
        run.open_round(None, true);
        assert_eq!(run.suggested_commit_message(), "Print No vehicles yet when the list is empty");
    }

    #[test]
    fn a_planned_round_uses_its_plan_summary() {
        let run = Run::new("/p", "Why?", Arc::new(|_: &str, _: &Event| {}));
        run.set_plan_summary("Add remove-car".to_string());
        run.set_tasks(vec![task("store part")]);
        run.open_round(None, false);
        assert_eq!(run.suggested_commit_message(), "Add remove-car");
    }
}
