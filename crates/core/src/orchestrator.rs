//! Decides which agent runs when.
//!
//! One project, one team. A round of work goes like this:
//!
//! ```text
//! your message -> the dispatcher: answers it, or passes it to the planner
//! planner: brainstorms with you, then makes a plan          (ask, chat, plan)
//! you approve -> workers build in parallel, each in its own worktree   (build)
//!             -> the reviewer checks all their work merged together    (review)
//! you: send work back to a worker, apply the changes, or discard them
//! ```
//!
//! Whenever nothing is running you can also message the planner, a worker or the
//! reviewer directly ([`Orchestrator::chat`]). That resumes the agent's own session,
//! so it remembers what it did and said.

use std::future::Future;
use std::sync::Arc;

use futures::future::join_all;
use serde::de::DeserializeOwned;

use crate::changes::{self, Changes};
use crate::config::{
    self, Plan, Review, Route, Routing, Task, DISPATCHER_ID, PLANNER_ID, REVIEWER_ID, WORKER_COUNT,
};
use crate::event::EventKind;
use crate::run::{Run, RunState};
use crate::runner::{run_agent, AgentError, AgentOutput, AgentRunner};
use crate::settings::{self, Settings, SharedSettings};
use crate::worktrees::Workspace;

/// Start of the prompt a worker gets when the reviewer sends its work back.
pub const FIX_PROMPT_MARKER: &str = "The reviewer asked for changes to your task.";
/// Longest message the user may send to an agent, in characters.
pub const MAX_MESSAGE_CHARS: usize = 20_000;

/// Where the workers do their work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Each worker gets its own git worktree; Apply brings the result into the
    /// project. Used for real runs.
    Worktrees,
    /// Everyone works directly in the project folder. Used by tests whose scripted
    /// agents never touch files, and by the planner-only example (planning reads only).
    SharedFolder,
}

/// Cheap to clone: it only holds shared pointers.
#[derive(Clone)]
pub struct Orchestrator {
    runner: Arc<dyn AgentRunner>,
    settings: SharedSettings,
    isolation: Isolation,
}

impl Orchestrator {
    pub fn new(runner: Arc<dyn AgentRunner>, settings: SharedSettings, isolation: Isolation) -> Self {
        Orchestrator { runner, settings, isolation }
    }

    /// The settings as they are right now. Read once per agent, so a change in the
    /// Settings window applies to the next agent that starts.
    fn settings(&self) -> Settings {
        settings::current(&self.settings)
    }

    // --- Talking ----------------------------------------------------------------------

    /// Give the user's message to the dispatcher, which answers it or passes it on to the
    /// planner (to brainstorm, or to plan). Check [`can_message`] first.
    pub async fn ask(&self, run: &Run, message: &str) -> Result<(), AgentError> {
        let spec = config::dispatcher(&self.settings());
        let previous = run.state();
        run.set_state(RunState::Chatting);
        run.emit(Some(DISPATCHER_ID), EventKind::UserMessage { text: message.to_string() });
        let session = run.session(DISPATCHER_ID);
        let output = run_agent(self.runner.as_ref(), run, &spec, message, session.as_deref()).await;
        run.set_state(previous);

        // A failed reply is already shown in the dispatcher's panel; the run carries on.
        let Ok(output) = output else { return Ok(()) };
        let Ok(routing) = structured_answer::<Routing>(run, DISPATCHER_ID, &output) else { return Ok(()) };
        run.emit(Some(DISPATCHER_ID), EventKind::Text { text: routing.reply });
        match routing.route {
            Route::Answer => Ok(()),
            Route::Brainstorm => self.chat(run, PLANNER_ID, message).await,
            Route::Plan => self.plan(run, Some(message)).await,
        }
    }

    /// Send the user's message to one agent and let it reply.
    ///
    /// Check [`can_chat`] first. A failed reply is shown in the agent's panel and
    /// the run goes back to the state it was in, so one bad answer does not end it.
    pub async fn chat(&self, run: &Run, agent_id: &str, message: &str) -> Result<(), AgentError> {
        let Some(spec) = config::chat_spec(&self.settings(), agent_id) else {
            return Err(AgentError(format!("There is no agent called {agent_id}.")));
        };
        let previous = run.state();
        run.set_state(RunState::Chatting);
        run.emit(Some(agent_id), EventKind::UserMessage { text: message.to_string() });

        let session = run.session(agent_id);
        let result = run_agent(self.runner.as_ref(), run, &spec, message, session.as_deref()).await;
        if let (Ok(output), Some(number)) = (&result, config::worker_number(agent_id)) {
            // Whatever the worker changed is saved on its branch, and what it said is
            // its latest report for the reviewer.
            run.record_worker(number, output.text.clone());
            if let Some(task) = run.task(number) {
                self.save_worker_changes(run, number, &task).await;
            }
        }
        run.set_state(previous);
        Ok(())
    }

    // --- Planning -----------------------------------------------------------------------

    /// The planner turns the conversation so far, or `request`, into tasks.
    pub async fn plan(&self, run: &Run, request: Option<&str>) -> Result<(), AgentError> {
        run.set_state(RunState::Planning);
        let settings = self.settings();
        let spec = config::planner(&settings);
        let request = request.map(str::trim).filter(|text| !text.is_empty());
        if let Some(text) = request {
            run.emit(Some(PLANNER_ID), EventKind::UserMessage { text: text.to_string() });
        }
        // If the user has already talked with the planner, continue that session so
        // the plan reflects the conversation.
        let session = run.session(PLANNER_ID);
        let prompt = match (&session, request) {
            (Some(_), Some(text)) => {
                format!("Turn what we discussed into the plan now. The user adds:\n\n{text}")
            }
            (Some(_), None) => "Turn what we discussed into the plan now.".to_string(),
            (None, Some(text)) => format!("Plan this request:\n\n{text}"),
            (None, None) => format!("Plan this request:\n\n{}", run.request),
        };
        let output = run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await?;

        let mut plan: Plan = structured_answer(run, PLANNER_ID, &output)?;
        // The schema already limits the tasks; this also covers a model that ignores it.
        plan.tasks.truncate(settings.worker_count);
        if plan.tasks.is_empty() {
            return Err(report(run, PLANNER_ID, "The planner returned no tasks."));
        }
        run.set_tasks(plan.tasks.clone());
        run.emit(Some(PLANNER_ID), EventKind::Plan { summary: plan.summary, tasks: plan.tasks });
        run.set_state(RunState::AwaitingApproval);
        Ok(())
    }

    // --- Building and reviewing ---------------------------------------------------------

    /// Open a round for the approved tasks: the workers build in parallel, then the
    /// reviewer checks their work. Check [`can_approve`] first.
    pub async fn build(&self, run: &Run, mut tasks: Vec<Task>) -> Result<(), AgentError> {
        tasks.truncate(WORKER_COUNT);
        let count = tasks.len();
        run.set_tasks(tasks);

        let workspace = match self.isolation {
            Isolation::SharedFolder => None,
            Isolation::Worktrees => {
                match Workspace::create(run.project(), &run.id, run.next_round_number(), count).await {
                    Ok(workspace) => Some(Arc::new(workspace)),
                    Err(problem) => {
                        // Nothing was started. The plan is still waiting, so the user can
                        // fix the problem (commit, say) and approve again.
                        run.emit(Some(PLANNER_ID), EventKind::Error { message: problem });
                        run.set_state(RunState::AwaitingApproval);
                        return Ok(());
                    }
                }
            }
        };
        run.open_round(workspace);

        run.set_state(RunState::Building);
        // `join_all` polls all these futures at the same time and waits for every one.
        // Each first waits its turn (see `start_after`), so the workers start a few
        // seconds apart and the later ones can reuse the earlier one's prompt cache.
        join_all((1..=count).map(|number| async move {
            self.start_after(number).await;
            self.build_one(run, number, false).await
        }))
        .await;

        self.review(run).await
    }

    /// Merge every finished worker's changes and let the reviewer check them all
    /// together. Also used for "Review again" after you talked to a worker.
    pub async fn review(&self, run: &Run) -> Result<(), AgentError> {
        run.set_state(RunState::Reviewing);
        let finished = run.finished_workers();
        if finished.is_empty() {
            let text = "No worker finished its task, so there is nothing to review.".to_string();
            run.emit(Some(REVIEWER_ID), EventKind::Notice { text });
            run.set_state(RunState::Done);
            return Ok(());
        }

        // A second review continues the first session, so it only needs the
        // changes made since then.
        let session = run.session(REVIEWER_ID);
        let since = session.as_ref().and_then(|_| run.last_reviewed());
        let (changes, reviewed_commit) = match run.workspace() {
            Some(workspace) => {
                if let Err(problem) = workspace.merge_into_review(&finished).await {
                    run.emit(Some(REVIEWER_ID), EventKind::Error { message: problem });
                    run.set_state(RunState::Done);
                    return Ok(());
                }
                (workspace.review_changes(since.as_deref()).await, workspace.review_commit().await.ok())
            }
            None => {
                let files: Vec<String> = run.tasks().into_iter().flat_map(|task| task.files).collect();
                (changes::changes_to(run.project(), &files).await, None)
            }
        };

        let spec = config::reviewer(&self.settings());
        let reports = reports_text(run, &finished);
        let prompt = match session {
            Some(_) => re_review_prompt(&reports, &changes),
            None => review_prompt(&run.tasks(), &reports, &changes),
        };
        if let Ok(output) = run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await {
            if let Ok(review) = structured_answer::<Review>(run, REVIEWER_ID, &output) {
                run.record_review(review.clone(), reviewed_commit);
                run.emit(Some(REVIEWER_ID), EventKind::Review(review));
            }
        }
        run.set_state(RunState::Done);
        Ok(())
    }

    /// Give worker `number` the reviewer's findings for its task, then review again.
    /// Check [`can_send_back`] first.
    pub async fn send_back(&self, run: &Run, number: usize) -> Result<(), AgentError> {
        run.set_state(RunState::Building);
        let text = "Sent back to fix what the reviewer found in this task.".to_string();
        run.emit(Some(&format!("worker-{number}")), EventKind::Notice { text });
        if self.build_one(run, number, true).await {
            return self.review(run).await;
        }
        run.set_state(RunState::Done);
        Ok(())
    }

    // --- Finishing a round ------------------------------------------------------------------

    /// Bring the round's changes into the project as staged changes, and close the
    /// round. If that is not possible, the round stays open and the reason is shown.
    pub async fn apply(&self, run: &Run) -> Result<(), AgentError> {
        let Some(workspace) = run.workspace() else {
            let text = "Round closed. Without worktrees the agents work in the project folder \
                        itself, so there was nothing to bring over."
                .to_string();
            run.emit(Some(REVIEWER_ID), EventKind::Notice { text });
            run.close_round();
            run.set_state(RunState::Ready);
            return Ok(());
        };
        match workspace.apply().await {
            Ok(files) => {
                let text = format!(
                    "Applied: {} file(s) are now staged in your project. Look them over with \
                     `git diff --staged`, then commit them.",
                    files.len()
                );
                run.emit(Some(REVIEWER_ID), EventKind::Notice { text });
                workspace.remove().await;
                run.close_round();
                run.set_state(RunState::Ready);
            }
            Err(problem) => run.emit(Some(REVIEWER_ID), EventKind::Error { message: problem }),
        }
        Ok(())
    }

    /// Throw the round's changes away: delete its worktrees and branches.
    pub async fn discard(&self, run: &Run) -> Result<(), AgentError> {
        if let Some(workspace) = run.close_round() {
            workspace.remove().await;
        }
        let text = "Discarded this round's changes. Your project folder was not touched.".to_string();
        run.emit(Some(REVIEWER_ID), EventKind::Notice { text });
        run.set_state(RunState::Ready);
        Ok(())
    }

    // --- Helpers --------------------------------------------------------------------------

    /// Wait before starting the `position`-th of several parallel agents (counting from 1).
    async fn start_after(&self, position: usize) {
        let earlier = u32::try_from(position.saturating_sub(1)).unwrap_or(u32::MAX);
        tokio::time::sleep(self.runner.parallel_start_gap().saturating_mul(earlier)).await;
    }

    /// Run worker `number` on its task (or on the reviewer's findings, for a fix) and
    /// save what it changed. Returns false if the worker failed.
    async fn build_one(&self, run: &Run, number: usize, is_fix: bool) -> bool {
        let spec = config::worker(&self.settings(), number);
        let Some(task) = run.task(number) else { return false };
        let (prompt, session) = match run.review() {
            Some(review) if is_fix => (fix_prompt(&review, number), run.session(&spec.id)),
            _ => (worker_prompt(&task), None),
        };
        match run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await {
            Ok(output) => {
                run.record_worker(number, output.text);
                self.save_worker_changes(run, number, &task).await;
                true
            }
            Err(_) => false, // run_agent already showed the error in the worker's panel
        }
    }

    /// Commit what worker `number` changed onto its branch, and point out any file it
    /// changed that was not in its task.
    async fn save_worker_changes(&self, run: &Run, number: usize, task: &Task) {
        let Some(workspace) = run.workspace() else { return };
        let agent = format!("worker-{number}");
        match workspace.commit_worker(number, &task.title, &task.files).await {
            Ok(commit) if !commit.outside_task.is_empty() => {
                let text = format!(
                    "Also changed files outside its task: {}. If another worker changed them too, \
                     merging their work will fail.",
                    commit.outside_task.join(", ")
                );
                run.emit(Some(&agent), EventKind::Notice { text });
            }
            Ok(_) => {}
            Err(problem) => {
                let message = format!("Could not save this worker's changes: {problem}");
                run.emit(Some(&agent), EventKind::Error { message });
            }
        }
    }
}

// --- What the user may do right now ---------------------------------------------------------
// The app checks these before starting anything, so a refused action never starts an
// agent. Each error is a sentence the user reads.

fn check_message(message: &str) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("Type a message first.".to_string());
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(format!("That message is too long (the limit is {MAX_MESSAGE_CHARS} characters)."));
    }
    Ok(())
}

fn check_at_rest(run: &Run) -> Result<(), String> {
    if run.is_at_rest() {
        Ok(())
    } else {
        Err("Wait until the agents have finished.".to_string())
    }
}

/// May the user send `message` to the dispatcher, or ask for a plan, now?
pub fn can_message(run: &Run, message: &str) -> Result<(), String> {
    check_message(message)?;
    check_at_rest(run)
}

/// May the user send `message` directly to `agent_id` now?
pub fn can_chat(run: &Run, agent_id: &str, message: &str) -> Result<(), String> {
    check_message(message)?;
    check_at_rest(run)?;
    if config::chat_spec(&Settings::default(), agent_id).is_none() {
        return Err(format!("You cannot message {agent_id} directly."));
    }
    // The planner can always be talked to. The others only during the round they
    // worked in, because a message continues their session in their worktree.
    if agent_id != PLANNER_ID && run.session(agent_id).is_none() {
        return Err("This agent has not worked on anything in the current round.".to_string());
    }
    Ok(())
}

/// May the user approve a plan now?
pub fn can_approve(run: &Run) -> Result<(), String> {
    if run.state() != RunState::AwaitingApproval || run.is_busy() {
        return Err("There is no plan waiting for approval.".to_string());
    }
    if run.has_open_round() {
        return Err("Apply or discard the previous round's changes first.".to_string());
    }
    Ok(())
}

/// May the user apply, discard, or review the open round again now?
pub fn can_finish_round(run: &Run) -> Result<(), String> {
    check_at_rest(run)?;
    if !run.has_open_round() {
        return Err("There are no changes waiting.".to_string());
    }
    Ok(())
}

/// May the user send the reviewer's findings back to worker `number` now?
pub fn can_send_back(run: &Run, number: usize) -> Result<(), String> {
    can_finish_round(run)?;
    let has_findings = run.review().is_some_and(|review| !review.findings_for(number).is_empty());
    if !has_findings {
        return Err(format!("The review has nothing for worker {number} to fix."));
    }
    if run.session(&format!("worker-{number}")).is_none() {
        return Err(format!("Worker {number} did not finish its task, so it cannot be resumed."));
    }
    Ok(())
}

/// Run one phase and make sure the run always ends in a clear state.
///
/// `select!` waits for whichever comes first: the work finishing, or the stop
/// signal. If the stop signal wins, the `work` future is dropped, which kills
/// any `claude` processes it had started (see `kill_on_drop` in `claude/mod.rs`).
pub async fn supervised<F>(run: Arc<Run>, work: F)
where
    F: Future<Output = Result<(), AgentError>>,
{
    let stop = run.begin_phase();
    let outcome = tokio::select! {
        result = work => Some(result),
        _ = stop => None,
    };
    run.end_phase();
    match outcome {
        Some(Ok(())) => {}
        Some(Err(_)) => run.set_state(RunState::Failed),
        None => run.set_state(RunState::Stopped),
    }
}

/// Turn the JSON answer of an agent that has an output schema into a Rust type.
///
/// Normally the CLI hands it over already parsed. As a fallback, look for a
/// JSON object in the agent's text.
fn structured_answer<T: DeserializeOwned>(
    run: &Run,
    agent: &str,
    output: &AgentOutput,
) -> Result<T, AgentError> {
    let value = match &output.structured {
        Some(value) => value.clone(),
        None => {
            let (start, end) = (output.text.find('{'), output.text.rfind('}'));
            let json = match (start, end) {
                (Some(start), Some(end)) if start < end => &output.text[start..=end],
                _ => return Err(report(run, agent, "The agent did not return a structured answer.")),
            };
            serde_json::from_str(json)
                .map_err(|_| report(run, agent, "The agent's structured answer was not valid JSON."))?
        }
    };
    serde_json::from_value(value)
        .map_err(|_| report(run, agent, "The agent's structured answer had the wrong shape."))
}

/// Show an error in an agent's panel and return it.
fn report(run: &Run, agent: &str, message: &str) -> AgentError {
    run.emit(Some(agent), EventKind::Error { message: message.to_string() });
    AgentError(message.to_string())
}

// --- Prompts -----------------------------------------------------------------------------

fn file_list(task: &Task) -> String {
    if task.files.is_empty() {
        "- (none listed)".to_string()
    } else {
        task.files.iter().map(|path| format!("- {path}")).collect::<Vec<_>>().join("\n")
    }
}

fn worker_prompt(task: &Task) -> String {
    format!(
        "Your task: {}\n\n{}\n\nFiles you may create or edit:\n{}",
        task.title,
        task.instructions,
        file_list(task)
    )
}

/// The finished workers' own reports, numbered by task.
fn reports_text(run: &Run, finished: &[usize]) -> String {
    finished
        .iter()
        .map(|&number| {
            let report = run.worker_report(number).filter(|text| !text.trim().is_empty());
            format!("Task {number}:\n{}", report.as_deref().unwrap_or("(no report)"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn changes_text(changes: &Changes) -> String {
    match changes {
        Changes::Diff(diff) => format!("The changes (git diff):\n\n{diff}"),
        Changes::Nothing => "Git shows no changes.".to_string(),
        Changes::TooLong => {
            "The changes are too long to include; use `git diff` and read the files.".to_string()
        }
        Changes::Unknown => "The changes are not included; read the tasks' files.".to_string(),
    }
}

fn review_prompt(tasks: &[Task], reports: &str, changes: &Changes) -> String {
    let tasks = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            format!("Task {}: {}\n{}\nFiles:\n{}", index + 1, task.title, task.instructions, file_list(task))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "Review the finished tasks below, which are merged in this folder.\n\n{tasks}\n\n\
         The workers' own reports:\n\n{reports}\n\n{}",
        changes_text(changes)
    )
}

fn re_review_prompt(reports: &str, changes: &Changes) -> String {
    format!(
        "The workers changed things since your last review. Review again: check that your earlier \
         findings are fixed and that nothing else broke, and give a complete verdict for all tasks.\n\n\
         Their latest reports:\n\n{reports}\n\nWhat changed since your last review:\n\n{}",
        changes_text(changes)
    )
}

fn fix_prompt(review: &Review, number: usize) -> String {
    let findings = review
        .findings_for(number)
        .iter()
        .map(|finding| format!("- {}", finding.issue))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{FIX_PROMPT_MARKER} Fix these and nothing else, then report what you changed.\n\n\
         Reviewer summary: {}\n\nMust fix in your task:\n{findings}",
        review.summary
    )
}
