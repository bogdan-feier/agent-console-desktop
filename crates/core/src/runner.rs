//! Running one agent, whatever is behind it.
//!
//! [`AgentRunner`] is a trait: a promise that a type can run an agent session.
//! Two types keep that promise. `claude::ClaudeRunner` starts the real `claude`
//! program; `demo::DemoRunner` plays a script. The orchestrator only knows the
//! trait, so it works with either.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::AgentSpec;
use crate::event::{AgentStatus, EventKind};
use crate::run::Run;

/// What an agent session produced.
#[derive(Debug, Clone, Default)]
pub struct AgentOutput {
    pub text: String,
    /// Filled in when the agent has an output schema.
    pub structured: Option<Value>,
    pub session_id: Option<String>,
}

/// The agent session ended with an error. The message is shown to the user.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct AgentError(pub String);

#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// How long to wait between starting two agents that run in parallel. Zero
    /// unless the runner benefits from a gap (see `PARALLEL_START_GAP` in config.rs).
    /// A method with a body like this is a default: types may override it.
    fn parallel_start_gap(&self) -> Duration {
        Duration::ZERO
    }

    /// Run one session to completion, emitting text and tool events on `run`.
    ///
    /// `resume` continues an earlier session, so the agent remembers its work.
    async fn run_session(
        &self,
        run: &Run,
        spec: &AgentSpec,
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<AgentOutput, AgentError>;
}

/// Run one agent and report its status (running, then done or error).
pub async fn run_agent(
    runner: &dyn AgentRunner,
    run: &Run,
    spec: &AgentSpec,
    prompt: &str,
    resume: Option<&str>,
) -> Result<AgentOutput, AgentError> {
    let mut status = StatusGuard::start(run, &spec.id);
    let result = runner.run_session(run, spec, prompt, resume).await;
    match &result {
        Ok(output) => {
            // Remember the session so the agent can be resumed later.
            if let Some(session) = &output.session_id {
                run.record_session(&spec.id, session.clone());
            }
            status.finish(AgentStatus::Done)
        }
        Err(error) => {
            run.emit(Some(&spec.id), EventKind::Error { message: error.to_string() });
            status.finish(AgentStatus::Error);
        }
    }
    result
}

/// Marks an agent as running, and guarantees it is marked as something else
/// afterwards.
///
/// When the user presses Stop, the future running the agent is dropped in the
/// middle of an `.await`, so no code after that line runs. `Drop` still does:
/// Rust calls it whenever a value goes away. That is where "stopped" is reported.
struct StatusGuard<'a> {
    run: &'a Run,
    agent: &'a str,
    finished: bool,
}

impl<'a> StatusGuard<'a> {
    fn start(run: &'a Run, agent: &'a str) -> Self {
        run.agent_started(agent);
        StatusGuard { run, agent, finished: false }
    }

    fn finish(&mut self, status: AgentStatus) {
        self.finished = true;
        self.run.agent_finished(self.agent, status);
    }
}

impl Drop for StatusGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.run.agent_finished(self.agent, AgentStatus::Stopped);
        }
    }
}
