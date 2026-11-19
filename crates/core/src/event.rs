//! What the UI is told while a run is in progress.
//!
//! Every event serialises to a flat JSON object such as
//! `{"seq": 4, "agent": "worker-1", "type": "tool", "name": "Read", "detail": "src/a.py"}`.

use serde::{Deserialize, Serialize};

use crate::config::{Review, Task};
use crate::run::RunState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Position in the run's event list, starting at 0.
    pub seq: usize,
    /// Which agent this is about; `None` for events about the whole run.
    pub agent: Option<String>,
    /// `flatten` puts the fields of `kind` directly into this JSON object.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// The `type` field of the JSON says which variant this is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    RunState {
        state: RunState,
    },
    AgentStatus {
        status: AgentStatus,
    },
    /// The exact model an alias such as "opus" resolved to.
    AgentModel {
        model: String,
    },
    /// Something the app itself wants to tell the user, shown in that agent's panel
    /// (for example, a worker that changed a file outside its task).
    Notice {
        text: String,
    },
    /// A worker changed files its task did not list. A list rather than a sentence, so
    /// the page can fold it: a worker that sets up a new project changes dozens.
    OutsideTask {
        files: Vec<String>,
    },
    /// A round of work (worktrees with changes) was opened or closed. While one is
    /// open, the page offers Apply, Discard and Review again.
    Round {
        open: bool,
        /// Opened by a quick change: one worker, and no reviewer will look at it.
        /// `#[serde(default)]` so rounds saved before this field existed still load.
        #[serde(default)]
        quick: bool,
        /// The title of each task, in worker order, so the window can label the
        /// panels without reading them back out of the plan card. A quick change
        /// has no plan card at all, and this is the only place its title comes from.
        #[serde(default)]
        tasks: Vec<String>,
    },
    /// A message the user sent to this agent.
    UserMessage {
        text: String,
    },
    /// Something the agent wrote.
    Text {
        text: String,
    },
    /// A tool the agent decided to call.
    Tool {
        name: String,
        detail: String,
    },
    /// A tool call that was refused or failed.
    ToolError {
        detail: String,
        /// When a shell command was refused: the rule that would allow commands
        /// like it, ready to be added to the allowed list. `None` for anything
        /// else, and for a command that failed on its own terms.
        allow_rule: Option<String>,
    },
    Error {
        message: String,
        /// When what stopped this was the project folder having uncommitted changes,
        /// the commit message to offer, so the window can put them away itself
        /// instead of sending the user to a terminal. `None` for anything else.
        ///
        /// `#[serde(default)]` so runs saved before this field existed still load.
        #[serde(default)]
        tidy_with: Option<String>,
    },
    /// The agent finished (or hit a limit); how much it used.
    Result {
        cost_usd: Option<f64>,
        turns: Option<u64>,
        tokens: TokenUsage,
    },
    Plan {
        summary: String,
        tasks: Vec<Task>,
    },
    /// The project's own check command, run by the app rather than by an agent.
    Checks {
        command: String,
        passed: bool,
        /// How it ended and how long it took.
        note: String,
        output: String,
    },
    Review(Review),
}

/// Tokens one session used, with the same field names the `claude` CLI reports.
///
/// Cache reads are the cheap kind: the part of the prompt that was the same as the
/// turn before. Cache creation is the prompt being stored for later turns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Done,
    Error,
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Verdict;
    use serde_json::json;

    #[test]
    fn events_serialise_to_flat_json() {
        let event = Event {
            seq: 4,
            agent: Some("worker-1".to_string()),
            kind: EventKind::Tool { name: "Read".to_string(), detail: "a.py".to_string() },
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"seq": 4, "agent": "worker-1", "type": "tool", "name": "Read", "detail": "a.py"})
        );
    }

    #[test]
    fn files_outside_a_task_are_saved_and_read_back_as_a_list() {
        let files = vec!["a.ts".to_string(), "b.ts".to_string()];
        let event =
            Event { seq: 2, agent: Some("worker-1".to_string()), kind: EventKind::OutsideTask { files } };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            json!({"seq": 2, "agent": "worker-1", "type": "outside_task", "files": ["a.ts", "b.ts"]})
        );
        let read: Event = serde_json::from_value(value).unwrap();
        assert!(matches!(read.kind, EventKind::OutsideTask { files } if files.len() == 2));
    }

    #[test]
    fn review_events_carry_the_review_fields() {
        let review = Review {
            verdict: Verdict::Approve,
            summary: "ok".to_string(),
            must_fix: vec![],
            consider: vec![],
            commit_message: "add the thing".to_string(),
        };
        let event = Event { seq: 0, agent: None, kind: EventKind::Review(review) };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "review");
        assert_eq!(value["verdict"], "approve");
        assert_eq!(value["commit_message"], "add the thing");
    }
}
