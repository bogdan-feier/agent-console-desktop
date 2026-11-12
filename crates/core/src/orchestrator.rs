//! Decides which agent runs when.
//!
//! The flow is fixed and written out top to bottom:
//!
//! ```text
//! planner -> (you approve the plan) -> workers in parallel -> reviewers in parallel
//! ```
//!
//! Each agent is its own session, so an agent only knows what we put in its prompt.

use std::future::Future;
use std::sync::Arc;

use futures::future::join_all;
use serde::de::DeserializeOwned;

use crate::config::{self, Plan, Review, Task, WORKER_COUNT};
use crate::event::EventKind;
use crate::run::{Run, RunState};
use crate::runner::{run_agent, AgentError, AgentOutput, AgentRunner};

/// Start of the prompt a worker gets when a reviewer sends its work back.
pub const FIX_PROMPT_MARKER: &str = "The reviewer asked for changes to your task.";

/// Cheap to clone: it only holds a shared pointer to the runner.
#[derive(Clone)]
pub struct Orchestrator {
    runner: Arc<dyn AgentRunner>,
}

impl Orchestrator {
    pub fn new(runner: Arc<dyn AgentRunner>) -> Self {
        Orchestrator { runner }
    }

    /// Phase 1: the planner explores the project and proposes tasks.
    pub async fn plan(&self, run: &Run) -> Result<(), AgentError> {
        run.set_state(RunState::Planning);
        let spec = config::planner();
        let prompt = format!("Plan this request:\n\n{}", run.request);
        let output = run_agent(self.runner.as_ref(), run, &spec, &prompt, None).await?;

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

    /// Phases 2 and 3: workers in parallel, then a reviewer for each worker that finished.
    pub async fn build_and_review(&self, run: &Run, mut tasks: Vec<Task>) -> Result<(), AgentError> {
        tasks.truncate(WORKER_COUNT);
        let numbers: Vec<usize> = (1..=tasks.len()).collect();
        run.set_tasks(tasks);

        run.set_state(RunState::Building);
        // `join_all` polls all these futures at the same time and waits for every one.
        let finished = join_all(numbers.iter().map(|&number| self.build_one(run, number, false))).await;

        run.set_state(RunState::Reviewing);
        let to_review = numbers.iter().zip(finished).filter(|(_, ok)| *ok).map(|(&number, _)| number);
        join_all(to_review.map(|number| self.review_one(run, number))).await;

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

    /// Run worker `number` on its task. Returns false if the worker failed.
    async fn build_one(&self, run: &Run, number: usize, is_fix: bool) -> bool {
        let spec = config::worker(number);
        let Some(task) = run.task(number) else { return false };
        let (prompt, session) = match run.review(number) {
            Some(review) if is_fix => (fix_prompt(&review), run.worker_session(number)),
            _ => (worker_prompt(&task), None),
        };
        match run_agent(self.runner.as_ref(), run, &spec, &prompt, session.as_deref()).await {
            Ok(output) => {
                run.record_worker(number, output.text, output.session_id);
                true
            }
            Err(_) => false, // run_agent already showed the error in the worker's panel
        }
    }

    /// Run reviewer `number` on what worker `number` did.
    async fn review_one(&self, run: &Run, number: usize) {
        let spec = config::reviewer(number);
        let Some(task) = run.task(number) else { return };
        let prompt = reviewer_prompt(&task, &run.worker_report(number).unwrap_or_default());
        let Ok(output) = run_agent(self.runner.as_ref(), run, &spec, &prompt, None).await else {
            return;
        };
        if let Ok(review) = structured_answer::<Review>(run, &spec.id, &output) {
            run.record_review(number, review.clone());
            run.emit(Some(&spec.id), EventKind::Review(review));
        }
    }
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

fn reviewer_prompt(task: &Task, report: &str) -> String {
    let report = if report.is_empty() { "(no report)" } else { report };
    format!("Review this finished task.\n\n{}\n\nThe worker's own report:\n{}", worker_prompt(task), report)
}

fn fix_prompt(review: &Review) -> String {
    let must_fix = review.must_fix.iter().map(|item| format!("- {item}")).collect::<Vec<_>>().join("\n");
    format!(
        "{FIX_PROMPT_MARKER} Fix these and nothing else, then report what you changed.\n\n\
         Reviewer summary: {}\n\nMust fix:\n{}",
        review.summary, must_fix
    )
}
