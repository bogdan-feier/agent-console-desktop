//! What the UI is told while a run is in progress.
//!
//! Every event serialises to a flat JSON object such as
//! `{"seq": 4, "agent": "worker-1", "type": "tool", "name": "Read", "detail": "src/a.py"}`.

use serde::Serialize;

use crate::config::{Review, Task};
use crate::run::RunState;

#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
    },
    Error {
        message: String,
    },
    /// The agent finished; how much it used.
    Result {
        cost_usd: Option<f64>,
        turns: Option<u64>,
    },
    Plan {
        summary: String,
        tasks: Vec<Task>,
    },
    Review(Review),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    fn review_events_carry_the_review_fields() {
        let review = Review {
            verdict: Verdict::Approve,
            summary: "ok".to_string(),
            must_fix: vec![],
            consider: vec![],
        };
        let event = Event { seq: 0, agent: None, kind: EventKind::Review(review) };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "review");
        assert_eq!(value["verdict"], "approve");
    }
}
