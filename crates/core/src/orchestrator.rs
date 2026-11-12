//! Decides which agent runs when.
//!
//! The main flow is fixed and written out top to bottom:
//!
//! ```text
//! (talk with the planner) -> planner makes a plan -> (you approve it)
//!     -> workers in parallel -> reviewers in parallel
//! ```
//!
//! Whenever nothing is running, the user can also send a message to any agent
//! that has worked before ([`Orchestrator::chat`]). That resumes the agent's own
//! session, so it remembers everything it did and said.

use std::future::Future;
use std::sync::Arc;

use futures::future::join_all;
use serde::de::DeserializeOwned;

use crate::changes::{self, Changes};
use crate::config::{self, Plan, Review, Role, Task, WORKER_COUNT};
use crate::event::EventKind;
use crate::run::{Run, RunState};
use crate::runner::{run_agent, AgentError, AgentOutput, AgentRunner};
use crate::settings::{self, Settings, SharedSettings};

/// Start of the prompt a worker gets when a reviewer sends its work back.
pub const FIX_PROMPT_MARKER: &str = "The reviewer asked for changes to your task.";
/// Longest message the user may send to an agent, in characters.
pub const MAX_MESSAGE_CHARS: usize = 20_000;

/// Cheap to clone: it only holds shared pointers.
#[derive(Clone)]
pub struct Orchestrator {
    runner: Arc<dyn AgentRunner>,
    settings: SharedSettings,
}

impl Orchestrator {
    pub fn new(runner: Arc<dyn AgentRunner>, settings: SharedSettings) -> Self {
        Orchestrator { runner, settings }
    }

    /// The settings as they are right now. Read once per agent, so a change in the
    /// Settings window applies to the next agent that starts.
    fn settings(&self) -> Settings {
        settings::current(&self.settings)
    }

    /// The planner turns the request, or the conversation so far, into tasks.
    ///
    /// `guidance` is optional extra direction typed by the user ("keep it to one file").
    pub async fn plan(&self, run: &Run, guidance: Option<&str>) -> Result<(), AgentError> {
        run.set_state(RunState::Planning);
        let spec = config::planner(&self.settings());
        // If the user has already talked with the planner, continue that session so
        // the plan reflects the conversation. Otherwise start from the request.
        let session = run.session(&spec.id);
        let mut prompt = match session {
            Some(_) => "Turn what we discussed into the plan now.".to_string(),
            None => format!("Plan this request:\n\n{}", run.request),
        };
        if let Some(guidance) = guidance.map(str::trim).filter(|text| !text.is_empty()) {
            run.emit(Some(&spec.id), EventKind::UserMessage { text: guidance.to_string() });
            prompt.push_str(&format!("\n\nAdditional direction from the user:\n{guidance}"));
        }
        let output = run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await?;

        let mut plan: Plan = structured_answer(run, &spec.id, &output)?;
        plan.tasks.truncate(WORKER_COUNT);
        if plan.tasks.is_empty() {
            return Err(report(run, &spec.id, "The planner returned no tasks."));
        }
        run.set_tasks(plan.tasks.clone());
        run.emit(Some(&spec.id), EventKind::Plan { summary: plan.summary, tasks: plan.tasks });
        run.set_state(RunState::AwaitingApproval);
        Ok(())
    }

    /// Workers in parallel, then a reviewer for each worker that finished.
    pub async fn build_and_review(&self, run: &Run, mut tasks: Vec<Task>) -> Result<(), AgentError> {
        tasks.truncate(WORKER_COUNT);
        let numbers: Vec<usize> = (1..=tasks.len()).collect();
        run.set_tasks(tasks);

        run.set_state(RunState::Building);
        // `join_all` polls all these futures at the same time and waits for every one.
        // Each one first waits its turn (see `start_after`), so the agents start a few
        // seconds apart and the later ones can reuse the earlier one's prompt cache.
        let finished = join_all(numbers.iter().map(|&number| async move {
            self.start_after(number).await;
            self.build_one(run, number, false).await
        }))
        .await;

        run.set_state(RunState::Reviewing);
        let to_review: Vec<usize> =
            numbers.iter().zip(finished).filter(|(_, ok)| *ok).map(|(&number, _)| number).collect();
        join_all(to_review.iter().enumerate().map(|(position, &number)| async move {
            self.start_after(position + 1).await;
            self.review_one(run, number).await
        }))
        .await;

        run.set_state(RunState::Done);
        Ok(())
    }

    /// Send a reviewer's findings back to the same worker, then review again.
    pub async fn fix(&self, run: &Run, number: usize) -> Result<(), AgentError> {
        run.set_state(RunState::Building);
        if self.build_one(run, number, true).await {
            run.set_state(RunState::Reviewing);
            self.review_one(run, number).await;
        }
        run.set_state(RunState::Done);
        Ok(())
    }

    /// Send the user's message to one agent and let it reply.
    ///
    /// Check [`can_chat`] first. A failed reply is shown in the agent's panel and
    /// the run goes back to the state it was in, so one bad answer does not end
    /// the run.
    pub async fn chat(&self, run: &Run, agent_id: &str, message: &str) -> Result<(), AgentError> {
        let Some(spec) = config::chat_spec(&self.settings(), agent_id) else {
            return Err(AgentError(format!("There is no agent called {agent_id}.")));
        };
        let previous = run.state();
        run.set_state(RunState::Chatting);
        run.emit(Some(agent_id), EventKind::UserMessage { text: message.to_string() });

        let session = run.session(agent_id);
        // The very first message of a brainstorm doubles as the run's request.
        let prompt = match (&session, spec.role) {
            (None, Role::Planner) => format!("{message}\n\n(The user may later ask you to plan this.)"),
            _ => message.to_string(),
        };
        let result = run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await;
        if let (Ok(output), Some(number)) = (&result, worker_number(agent_id)) {
            // What a worker says in a chat is its latest report for the reviewer.
            run.record_worker(number, output.text.clone());
        }
        run.set_state(previous);
        Ok(())
    }

    /// Wait before starting the `position`-th of several parallel agents (counting from 1).
    async fn start_after(&self, position: usize) {
        let earlier = u32::try_from(position.saturating_sub(1)).unwrap_or(u32::MAX);
        tokio::time::sleep(self.runner.parallel_start_gap().saturating_mul(earlier)).await;
    }

    /// Run worker `number` on its task. Returns false if the worker failed.
    async fn build_one(&self, run: &Run, number: usize, is_fix: bool) -> bool {
        let spec = config::worker(&self.settings(), number);
        let Some(task) = run.task(number) else { return false };
        let (prompt, session) = match run.review(number) {
            Some(review) if is_fix => (fix_prompt(&review), run.session(&spec.id)),
            _ => (worker_prompt(&task), None),
        };
        match run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await {
            Ok(output) => {
                run.record_worker(number, output.text);
                true
            }
            Err(_) => false, // run_agent already showed the error in the worker's panel
        }
    }

    /// Run reviewer `number` on what worker `number` did.
    async fn review_one(&self, run: &Run, number: usize) {
        let spec = config::reviewer(&self.settings(), number);
        let Some(task) = run.task(number) else { return };
        let changes = changes::changes_to(&run.project_dir, &task.files).await;
        let prompt = reviewer_prompt(&task, &run.worker_report(number).unwrap_or_default(), &changes);
        let Ok(output) = run_agent(self.runner.as_ref(), run, &spec, &prompt, None).await else {
            return;
        };
        if let Ok(review) = structured_answer::<Review>(run, &spec.id, &output) {
            run.record_review(number, review.clone());
            run.emit(Some(&spec.id), EventKind::Review(review));
        }
    }
}

/// Whether the user may send `message` to `agent_id` right now. The error says why not.
pub fn can_chat(run: &Run, agent_id: &str, message: &str) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("Type a message first.".to_string());
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(format!("That message is too long (the limit is {MAX_MESSAGE_CHARS} characters)."));
    }
    if !run.is_at_rest() {
        return Err("Wait until the agents have finished.".to_string());
    }
    if config::chat_spec(&Settings::default(), agent_id).is_none() {
        return Err(format!("There is no agent called {agent_id}."));
    }
    // The planner can always be talked to. The others only once they have worked,
    // because a message continues their earlier session.
    if agent_id != "planner" && run.session(agent_id).is_none() {
        return Err("This agent has not worked on anything yet.".to_string());
    }
    Ok(())
}

/// "worker-2" -> Some(2); anything else -> None.
fn worker_number(agent_id: &str) -> Option<usize> {
    agent_id.strip_prefix("worker-")?.parse().ok()
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

fn worker_prompt(task: &Task) -> String {
    let files = if task.files.is_empty() {
        "- (none listed)".to_string()
    } else {
        task.files.iter().map(|path| format!("- {path}")).collect::<Vec<_>>().join("\n")
    };
    format!("Your task: {}\n\n{}\n\nFiles you may create or edit:\n{}", task.title, task.instructions, files)
}

fn reviewer_prompt(task: &Task, report: &str, changes: &Changes) -> String {
    let report = if report.is_empty() { "(no report)" } else { report };
    let changes = match changes {
        Changes::Diff(diff) => {
            format!("The changes to the task's files since the last commit (new files in full):\n\n{diff}")
        }
        Changes::Nothing => "Git shows no changes to the task's files.".to_string(),
        Changes::TooLong => "The changes are too long to include; read the task's files.".to_string(),
        Changes::Unknown => "The changes are not included; read the task's files.".to_string(),
    };
    format!(
        "Review this finished task.\n\n{}\n\nThe worker's own report:\n{}\n\n{}",
        worker_prompt(task),
        report,
        changes
    )
}

fn fix_prompt(review: &Review) -> String {
    let must_fix = review.must_fix.iter().map(|item| format!("- {item}")).collect::<Vec<_>>().join("\n");
    format!(
        "{FIX_PROMPT_MARKER} Fix these and nothing else, then report what you changed.\n\n\
         Reviewer summary: {}\n\nMust fix:\n{}",
        review.summary, must_fix
    )
}
