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
use std::path::Path;
use std::sync::Arc;

use futures::future::join_all;
use serde::de::DeserializeOwned;

use crate::changes::{self, Changes};
use crate::checks::{self, Checked};
use crate::config::{
    self, Plan, Review, Route, Routing, Task, DISPATCHER_ID, PLANNER_ID, RESOLVER_ID, REVIEWER_ID,
    WORKER_COUNT, WORKER_ONE,
};
use crate::event::EventKind;
use crate::run::{Run, RunState};
use crate::runner::{run_agent, AgentError, AgentOutput, AgentRunner};
use crate::settings::{self, Settings, SharedSettings};
use crate::worktrees::{self, FileChange, Merge, Workspace};

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

/// What `settle` produced: the round's changes, where they sit, and how the
/// project's checks went.
struct Settled {
    changes: Changes,
    commit: Option<String>,
    checked: Option<Checked>,
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
            // A quick change opens a round of its own, and only one can be open. Planning
            // does not need one, so the request still goes somewhere useful.
            Route::Quick if run.has_open_round() => {
                let text = "A quick change needs the open round to be applied or discarded first, so \
                            the planner is planning this one instead."
                    .to_string();
                run.emit(None, EventKind::Notice { text });
                self.plan(run, Some(message)).await
            }
            Route::Quick => self.quick(run, message).await,
        }
    }

    /// Send the user's message to one agent and let it reply.
    ///
    /// Check [`can_chat`] first. A failed reply is shown in the agent's panel and
    /// the run goes back to the state it was in, so one bad answer does not end it.
    pub async fn chat(&self, run: &Run, agent_id: &str, message: &str) -> Result<(), AgentError> {
        let Some(mut spec) = config::chat_spec(&self.settings(), agent_id) else {
            return Err(AgentError(format!("There is no agent called {agent_id}.")));
        };
        // A worker carries on in the model its task ran on, so a hard task's session
        // does not drop back to Haiku halfway through.
        if let Some(task) = config::worker_number(agent_id).and_then(|number| run.task(number)) {
            spec = config::for_task(spec, &task);
        }
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
        let previous = run.state();
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
        // No tasks is an answer, not a failure: the message was a question or asked for
        // nothing to change (Plan is easy to leave selected). Its summary says why.
        if plan.tasks.is_empty() {
            let text = if plan.summary.trim().is_empty() {
                "Nothing to plan here: this needs no change to the project.".to_string()
            } else {
                plan.summary
            };
            run.emit(Some(PLANNER_ID), EventKind::Text { text });
            run.set_state(previous);
            return Ok(());
        }
        run.set_tasks(plan.tasks.clone());
        run.set_plan_summary(plan.summary.clone());
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
                        let tidy_with = tidy_offer(&problem, run.rounds_started());
                        run.emit(Some(PLANNER_ID), EventKind::Error { message: problem, tidy_with });
                        run.set_state(RunState::AwaitingApproval);
                        return Ok(());
                    }
                }
            }
        };
        run.open_round(workspace, false);

        run.set_state(RunState::Building);
        // `join_all` polls all these futures at the same time and waits for every one.
        // Each first waits its turn (see `start_after`), so the workers start a few
        // seconds apart and the later ones can reuse the earlier one's prompt cache.
        join_all((1..=count).map(|number| async move {
            self.start_after(number).await;
            self.build_one(run, number, Job::Task).await
        }))
        .await;

        // Only here may Settings skip the review: "Review again" and a send-back are
        // the user asking for one.
        self.review_round(run, true).await
    }

    /// Merge every finished worker's changes and let the reviewer check them all
    /// together. Also used for "Review again" after you talked to a worker.
    pub async fn review(&self, run: &Run) -> Result<(), AgentError> {
        self.review_round(run, false).await
    }

    /// [`Self::review`], which may end without the reviewer when `may_skip` and
    /// Settings says to skip the review once the checks pass.
    async fn review_round(&self, run: &Run, may_skip: bool) -> Result<(), AgentError> {
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
        let Some(settled) = self.settle(run, &finished, since.as_deref(), REVIEWER_ID).await else {
            return Ok(());
        };
        let Settled { changes, commit: reviewed_commit, checked } = settled;
        let checks_passed = checked.as_ref().is_some_and(|checked| checked.passed);
        if may_skip && checks_passed && self.settings().skip_review_when_checks_pass {
            let text = "The project's checks passed, so the review was skipped, as Settings asks. Look at \
                        the changes before you apply them, or press Review again to have them reviewed."
                .to_string();
            run.emit(Some(REVIEWER_ID), EventKind::Notice { text });
            run.set_state(RunState::Done);
            return Ok(());
        }

        let spec = config::reviewer(&self.settings());
        let reports = reports_text(run, &finished);
        let prompt = match session {
            Some(_) => re_review_prompt(&reports, &changes),
            None => review_prompt(&run.tasks(), &reports, &changes),
        } + &checks_text(checked.as_ref())
            + &commit_style(&worktrees::recent_subjects(run.project()).await);
        if let Ok(output) = run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await {
            if let Ok(review) = structured_answer::<Review>(run, REVIEWER_ID, &output) {
                run.record_review(review.clone(), reviewed_commit);
                run.emit(Some(REVIEWER_ID), EventKind::Review(review));
            }
        }
        run.set_state(RunState::Done);
        Ok(())
    }

    /// One worker, no planner and no reviewer: the cheap path for a small change.
    ///
    /// A full round is four Claude Code sessions, and each one pays for its own system
    /// prompt and tool descriptions before it does anything. That is worth it when
    /// several workers touch several files and a second pair of eyes is worth having.
    /// For a one-function change it is not, so this runs the user's words as the only
    /// task and stops after the project's own checks. What it gives up is the review:
    /// the changes still go nowhere until the user has looked at the diff and applied
    /// them.
    pub async fn quick(&self, run: &Run, message: &str) -> Result<(), AgentError> {
        run.emit(None, EventKind::UserMessage { text: message.to_string() });
        run.set_tasks(vec![Task {
            title: first_line(message),
            instructions: message.to_string(),
            files: Vec::new(),
            // Nothing was read beforehand: a quick change has no planner.
            context: String::new(),
            // Quick is the cheap path for small changes, so it stays on the workers' model.
            hard: false,
        }]);

        let workspace = match self.isolation {
            Isolation::SharedFolder => None,
            Isolation::Worktrees => {
                match Workspace::create(run.project(), &run.id, run.next_round_number(), 1).await {
                    Ok(workspace) => Some(Arc::new(workspace)),
                    Err(problem) => {
                        let tidy_with = tidy_offer(&problem, run.rounds_started());
                        run.emit(Some(WORKER_ONE), EventKind::Error { message: problem, tidy_with });
                        run.set_state(RunState::Ready);
                        return Ok(());
                    }
                }
            }
        };
        run.open_round(workspace, true);

        run.set_state(RunState::Building);
        self.build_one(run, 1, Job::Task).await;

        let finished = run.finished_workers();
        if finished.is_empty() {
            run.set_state(RunState::Done);
            return Ok(());
        }
        if let Some(settled) = self.settle(run, &finished, None, WORKER_ONE).await {
            let text = quick_notice(settled.checked.as_ref().map(|checked| checked.passed)).to_string();
            run.emit(None, EventKind::Notice { text });
            run.set_state(RunState::Done);
        }
        Ok(())
    }

    /// Give worker `number` the reviewer's findings for its task, then review again.
    /// `number` 0 means the findings in files no task listed: they go to the first
    /// worker that finished, which may then edit the files they name.
    /// Check [`can_send_back`] first.
    pub async fn send_back(&self, run: &Run, number: usize) -> Result<(), AgentError> {
        let (worker, job, text) = if number == 0 {
            let Some(worker) = unassigned_handler(run) else { return Ok(()) };
            (worker, Job::FixUnassigned, "Given the findings no task covered, to fix in their files.")
        } else {
            (number, Job::FixOwn, "Sent back to fix what the reviewer found in this task.")
        };
        run.set_state(RunState::Building);
        run.emit(Some(&format!("worker-{worker}")), EventKind::Notice { text: text.to_string() });
        if self.build_one(run, worker, job).await {
            return self.review(run).await;
        }
        run.set_state(RunState::Done);
        Ok(())
    }

    // --- Finishing a round ------------------------------------------------------------------

    /// Bring the round's changes into the project as staged changes, and close the
    /// round. If that is not possible, the round stays open and the reason is shown.
    pub async fn apply(&self, run: &Run, commit_message: Option<&str>, push: bool) -> Result<(), AgentError> {
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
                let count = crate::plural(files.len(), "file");
                let is_are = if files.len() == 1 { "is" } else { "are" };
                let text = match commit_message {
                    None => format!(
                        "Applied: {count} {is_are} now staged in your project. Look them over with \
                         `git diff --staged`, then commit them."
                    ),
                    Some(message) => match workspace.commit(message).await {
                        Ok(()) => {
                            let done = format!("Applied and committed {count} to your project.");
                            match push {
                                false => done,
                                true => match workspace.push().await {
                                    Ok(branch) if branch.is_empty() => {
                                        format!("{done} Pushed.")
                                    }
                                    Ok(branch) => format!("{done} Pushed {branch}."),
                                    Err(problem) => format!(
                                        "{done} The push did not go through ({problem}). The \
                                         commit is there; push it when you have sorted that out."
                                    ),
                                },
                            }
                        }
                        Err(problem) => format!(
                            "Applied: {count} {is_are} staged in your project, but the commit did not \
                             go through ({problem}). Commit them yourself when you have fixed that."
                        ),
                    },
                };
                run.emit(Some(REVIEWER_ID), EventKind::Notice { text });
                workspace.remove().await;
                run.close_round();
                run.set_state(RunState::Ready);
            }
            Err(problem) => {
                let tidy_with = tidy_offer(&problem, run.rounds_started());
                run.emit(Some(REVIEWER_ID), EventKind::Error { message: problem, tidy_with });
            }
        }
        Ok(())
    }

    /// Every file the open round would change, for the window that shows them
    /// before they are applied.
    pub async fn round_changes(&self, run: &Run) -> Result<Vec<FileChange>, String> {
        match run.workspace() {
            Some(workspace) => workspace.file_changes().await,
            None => Err("This round has no separate copy of the project to compare.".to_string()),
        }
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

    /// Merge every finished worker's branch into the review worktree, asking the
    /// resolver to step in when two workers changed the same lines.
    ///
    /// `merge_into_review` skips branches that are already in, so after a clash is
    /// resolved we simply try the whole list again. Each round of the loop ends one
    /// clash, and there cannot be more clashes than there are branches.
    /// Put the finished workers' changes together and run the project's checks.
    ///
    /// The half of finishing a round that costs nothing: git and the project's own
    /// check command, no agent. Both finishing paths start here, the reviewed one
    /// and the quick one. `None` means it could not be done and the round is over;
    /// the reason is already on screen, under `blame`.
    async fn settle(
        &self,
        run: &Run,
        finished: &[usize],
        since: Option<&str>,
        blame: &str,
    ) -> Option<Settled> {
        let Some(workspace) = run.workspace() else {
            let files: Vec<String> = run.tasks().into_iter().flat_map(|task| task.files).collect();
            return Some(Settled {
                changes: changes::changes_to(run.project(), &files).await,
                commit: None,
                checked: None,
            });
        };
        if let Err(problem) = self.merge_all(run, &workspace, finished).await {
            run.emit(Some(blame), EventKind::Error { message: problem, tidy_with: None });
            run.set_state(RunState::Done);
            return None;
        }
        Some(Settled {
            checked: self.run_checks(run, workspace.review_dir()).await,
            changes: workspace.review_changes(since).await,
            commit: workspace.review_commit().await.ok(),
        })
    }

    async fn merge_all(&self, run: &Run, workspace: &Workspace, finished: &[usize]) -> Result<(), String> {
        for _ in 0..=finished.len() {
            match workspace.merge_into_review(finished).await? {
                Merge::Clean => return Ok(()),
                Merge::Conflicted { worker, files } => {
                    if !self.settings().resolve_conflicts {
                        workspace.abort_merge().await;
                        return Err(clash_message(worker, &files, "it is turned off in Settings"));
                    }
                    self.resolve(run, workspace, worker, &files).await?;
                }
            }
        }
        Err("The workers' changes kept clashing, so the round was left as it was.".to_string())
    }

    /// Let the resolver make one clash whole again. The merge is in progress while
    /// this runs: it either ends in a commit or is undone completely.
    async fn resolve(
        &self,
        run: &Run,
        workspace: &Workspace,
        worker: usize,
        files: &[String],
    ) -> Result<(), String> {
        let text = format!(
            "Worker {worker} changed the same lines as another worker in {}. Merging the two versions.",
            files.join(", ")
        );
        run.emit(Some(RESOLVER_ID), EventKind::Notice { text });

        let spec = config::resolver(&self.settings());
        let prompt = resolve_prompt(files);
        let outcome = run_agent(self.runner.as_ref(), run, &spec, &prompt, None).await;
        if outcome.is_err() {
            workspace.abort_merge().await;
            return Err(clash_message(worker, files, "the resolver could not finish"));
        }
        match workspace.finish_merge(worker, files, "resolved by the resolver").await {
            Ok(()) => {
                let text = format!(
                    "Merged the two versions of {}. The reviewer checks the result next, and you see \
                     every line before it reaches your project.",
                    files.join(", ")
                );
                run.emit(Some(RESOLVER_ID), EventKind::Notice { text });
                Ok(())
            }
            Err(problem) => {
                workspace.abort_merge().await;
                Err(clash_message(worker, files, &problem))
            }
        }
    }

    /// Run the project's own check command on the merged changes, if there is one.
    /// The reviewer is given the output, which saves it the turns.
    async fn run_checks(&self, run: &Run, dir: std::path::PathBuf) -> Option<Checked> {
        let command = self.settings().check_command;
        if command.trim().is_empty() {
            return None;
        }
        let checked = checks::run(&dir, &command).await;
        run.emit(
            None,
            EventKind::Checks {
                command: checked.command.clone(),
                passed: checked.passed,
                note: checked.note.clone(),
                output: checked.output.clone(),
            },
        );
        Some(checked)
    }

    /// Wait before starting the `position`-th of several parallel agents (counting from 1).
    async fn start_after(&self, position: usize) {
        let earlier = u32::try_from(position.saturating_sub(1)).unwrap_or(u32::MAX);
        tokio::time::sleep(self.runner.parallel_start_gap().saturating_mul(earlier)).await;
    }

    /// Run worker `number` on its task (or on the reviewer's findings, for a fix) and
    /// save what it changed. Returns false if the worker failed.
    async fn build_one(&self, run: &Run, number: usize, job: Job) -> bool {
        let Some(task) = run.task(number) else { return false };
        let spec = config::for_task(config::worker(&self.settings(), number), &task);
        let (prompt, session) = match (run.review(), job) {
            (Some(review), Job::FixOwn) => (fix_prompt(&review, number), run.session(&spec.id)),
            (Some(review), Job::FixUnassigned) => (unassigned_fix_prompt(&review), run.session(&spec.id)),
            _ => (worker_prompt(&task, run.round_is_quick()), None),
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
            // In a quick change there is no file list and no other worker, so there is
            // no "outside its task" to be outside of.
            Ok(commit) if !commit.outside_task.is_empty() && !run.round_is_quick() => {
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
                run.emit(Some(&agent), EventKind::Error { message, tidy_with: None });
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

/// May a new run start in `project_dir`, given the runs the app has open?
///
/// Only a run in the same project with changes still waiting stands in the way:
/// applying one round's changes and then another's onto the same folder would make
/// the second fail. A round in another project works in its own copies and applies
/// to its own folder, so it does not block anything; it stays saved, and opening it
/// from Runs brings its changes back.
pub fn can_start_run<'a>(
    project_dir: &Path,
    open_runs: impl IntoIterator<Item = &'a Run>,
) -> Result<(), String> {
    // Compare real locations, so "~/glovebox" and "/Users/me/glovebox/" are one project.
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let here = real(project_dir);
    match open_runs.into_iter().find(|run| run.has_open_round() && real(run.project()) == here) {
        Some(waiting) => Err(format!(
            "A run in this project still has changes waiting: \"{}\". Open it from Runs and apply or \
             discard them first.",
            first_line(&waiting.request)
        )),
        None => Ok(()),
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

/// May the user send the reviewer's findings back to worker `number` now? `number` 0
/// is the findings no task covered (see [`Orchestrator::send_back`]).
pub fn can_send_back(run: &Run, number: usize) -> Result<(), String> {
    can_finish_round(run)?;
    if number == 0 {
        if !run.review().is_some_and(|review| !review.for_the_user().is_empty()) {
            return Err("The review has nothing outside the tasks' files to fix.".to_string());
        }
        if unassigned_handler(run).is_none() {
            return Err("No worker finished its task in this round, so there is nobody to give \
                        these to. They are yours to do."
                .to_string());
        }
        return Ok(());
    }
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
    run.emit(Some(agent), EventKind::Error { message: message.to_string(), tidy_with: None });
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

/// What a worker is allowed to touch.
///
/// In a planned round it is the planner's list, and a worker that needs a file not
/// on it is meant to stop: the list is how two workers are kept out of each other's
/// way. A quick change has no plan and so no list, and the same rule there would
/// stop every worker before it started. It is the only task in the round, so there
/// is nobody to collide with and it can decide for itself.
fn files_allowed(task: &Task, free_rein: bool) -> String {
    if free_rein {
        "Files you may create or edit: whichever ones the task needs. Nobody else is \
         working on this project right now, so there is no list. Change as little as \
         the task allows, and say in your report what you touched."
            .to_string()
    } else {
        format!("Files you may create or edit:\n{}", file_list(task))
    }
}

fn worker_prompt(task: &Task, free_rein: bool) -> String {
    format!(
        "Your task: {}\n\n{}\n\n{}{}",
        task.title,
        task.instructions,
        files_allowed(task, free_rein),
        planner_context(task)
    )
}

/// What the planner found, handed to the worker so it does not read the project a
/// second time. Said to be second-hand on purpose: it is a summary written by
/// someone else, and a worker that trusts a wrong line without checking is worse
/// than one that reads the file.
fn planner_context(task: &Task) -> String {
    let found = task.context.trim();
    if found.is_empty() {
        return String::new();
    }
    format!(
        "\n\nWhat the planner found while reading the project, so you do not have to \
         look it up again. It is a summary, not the files themselves: work from it, and \
         read the file yourself if something does not match or is missing.\n\n{found}"
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

/// What the resolver is told. The files are already in its folder with git's
/// markers in them, so it only needs to know which ones.
fn resolve_prompt(files: &[String]) -> String {
    let list = files.iter().map(|path| format!("- {path}")).collect::<Vec<_>>().join("\n");
    format!(
        "Two workers changed the same lines. These files are in your folder with git's conflict \
         markers in them:\n\n{list}\n\nResolve every marker so that both workers' work survives, and \
         change nothing else. Then report what each side wanted and what you did."
    )
}

/// What the user is told when a clash could not be merged.
fn clash_message(worker: usize, files: &[String], why: &str) -> String {
    format!(
        "Worker {worker}'s changes clash with another worker's in {}, and they were not merged \
         ({why}). Nothing was lost: send one worker back to undo its change there, or discard the \
         round and plan again with tasks that do not share files.",
        files.join(", ")
    )
}

/// The check output, for the end of the reviewer's prompt.
/// What a finished quick change says, from whether the project's checks passed
/// (`None`: no check command is set). A quick change has no reviewer, and its worker
/// only runs its own tests, so without a check command nothing has run the whole
/// suite: a change that breaks an older test elsewhere would go unnoticed.
fn quick_notice(checks_passed: Option<bool>) -> &'static str {
    match checks_passed {
        Some(false) => {
            "Done, but the project's checks did not pass. Nothing was reviewed: look at the \
             changes before you apply them."
        }
        Some(true) => {
            "Done, and the project's checks passed. Nobody reviewed this, so look at the changes \
                       before you apply them."
        }
        None => {
            "Done. Nobody reviewed this, and no check command is set, so the whole test suite has not run \
             on it either. Look at the changes before you apply them, and set a check command in Settings \
             to have the tests run after every change."
        }
    }
}

fn checks_text(checked: Option<&Checked>) -> String {
    let Some(checked) = checked else { return String::new() };
    let verdict = if checked.passed { "passed" } else { "FAILED" };
    format!(
        "\n\nThe project's own check `{}` was run on these changes and {verdict} ({}). You do not \
         need to run it again. Its output:\n\n{}",
        checked.command,
        checked.note,
        if checked.output.trim().is_empty() { "(nothing printed)" } else { &checked.output }
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

/// What a worker is asked to do in [`Orchestrator::build_one`].
#[derive(Clone, Copy, PartialEq)]
enum Job {
    /// Its task from the plan (or the quick change).
    Task,
    /// The reviewer's findings for its own task.
    FixOwn,
    /// The findings no task covered, in files outside its own list.
    FixUnassigned,
}

/// The worker that gets the findings no task covered: the first one that finished,
/// because it has a session to continue and already knows the project.
fn unassigned_handler(run: &Run) -> Option<usize> {
    run.finished_workers().into_iter().find(|number| run.session(&format!("worker-{number}")).is_some())
}

/// The fix prompt for findings in files no task listed. A worker is told to stop at a
/// file outside its list, so this says plainly that these files are allowed.
fn unassigned_fix_prompt(review: &Review) -> String {
    let findings = review
        .for_the_user()
        .iter()
        .map(|finding| format!("- {}", finding.issue))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{FIX_PROMPT_MARKER} The reviewer found these in files that no task listed. You may create or \
         edit the files they name, and only those. Fix these and nothing else, then report what you \
         changed.\n\nReviewer summary: {}\n\nMust fix:\n{findings}",
        review.summary
    )
}

/// A commit message to offer, when the problem was the project folder being untidy.
///
/// The window turns this into a button, so nobody has to open a terminal to get past
/// it. The wording matched here is `worktrees::dirty_message`, which both checks use.
fn tidy_offer(problem: &str, rounds: usize) -> Option<String> {
    if !problem.contains("uncommitted changes") {
        return None;
    }
    Some(match rounds {
        0 => "My own changes, before the agents start".to_string(),
        round => format!("My own changes, before applying round {round}"),
    })
}

/// The project's own commit subjects, so the reviewer's commit message reads like the
/// ones already in the log rather than like a machine's.
fn commit_style(subjects: &[String]) -> String {
    if subjects.is_empty() {
        return String::new();
    }
    let examples = subjects.iter().map(|line| format!("- {line}")).collect::<Vec<_>>().join("\n");
    format!("\n\nThis project's recent commit subjects, for the style of yours:\n{examples}")
}

/// The first line of the user's message, for the task's title.
fn first_line(message: &str) -> String {
    message.lines().find(|line| !line.trim().is_empty()).unwrap_or(message).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_waiting_changes_in_the_same_project_stop_a_new_run() {
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let quiet: crate::run::EventSink = Arc::new(|_: &str, _: &crate::Event| {});
        let other_project = Run::new(elsewhere.path(), "Make the logo blue", quiet.clone());
        other_project.open_round(None, false);
        assert!(can_start_run(here.path(), [other_project.as_ref()]).is_ok(), "another project's round");

        let settled = Run::new(here.path(), "An older run", quiet.clone());
        assert!(can_start_run(here.path(), [settled.as_ref()]).is_ok(), "nothing waiting");

        let waiting = Run::new(here.path(), "Recolour X and O\nwith details", quiet);
        waiting.open_round(None, false);
        let refused = can_start_run(here.path(), [other_project.as_ref(), waiting.as_ref()]).unwrap_err();
        assert!(refused.contains("\"Recolour X and O\""), "names the run that waits: {refused}");
    }

    #[test]
    fn a_quick_change_without_a_check_command_says_the_tests_did_not_run() {
        assert!(quick_notice(None).contains("no check command is set"));
        assert!(quick_notice(Some(true)).contains("checks passed"));
        assert!(quick_notice(Some(false)).contains("did not pass"));
    }

    #[test]
    fn a_worker_is_told_what_the_planner_already_read() {
        let task = Task {
            title: "Add farewell()".to_string(),
            instructions: "Add it next to greet.".to_string(),
            files: vec!["app.py".to_string()],
            context: "greet(name) is at app.py:12 and returns an f-string. Tests use \
                      plain asserts, no fixtures."
                .to_string(),
            hard: false,
        };

        let prompt = worker_prompt(&task, false);

        assert!(prompt.contains("greet(name) is at app.py:12"), "{prompt}");
        assert!(prompt.contains("read the file yourself if something does not match"), "{prompt}");
    }

    #[test]
    fn a_quick_change_lets_its_worker_choose_the_files() {
        // No plan means no file list, and a worker told to stop at an empty list
        // would never start.
        let task = Task {
            title: "Make the warning distance configurable".to_string(),
            instructions: "Read it from the environment.".to_string(),
            files: Vec::new(),
            context: String::new(),
            hard: false,
        };

        let quick = worker_prompt(&task, true);
        assert!(quick.contains("whichever ones the task needs"), "{quick}");
        assert!(!quick.contains("(none listed)"), "{quick}");

        // A planned task with no files is a different thing: the planner should have
        // listed them, and stopping is the right answer.
        let planned = worker_prompt(&task, false);
        assert!(planned.contains("(none listed)"), "{planned}");
    }

    #[test]
    fn a_task_with_nothing_found_says_nothing_about_it() {
        let task = Task {
            title: "Add farewell()".to_string(),
            instructions: "Add it next to greet.".to_string(),
            files: vec!["app.py".to_string()],
            context: "   ".to_string(),
            hard: false,
        };

        let prompt = worker_prompt(&task, false);

        assert!(!prompt.contains("the planner found"), "no empty heading: {prompt}");
        assert!(prompt.trim_end().ends_with("app.py"), "{prompt}");
    }
}
