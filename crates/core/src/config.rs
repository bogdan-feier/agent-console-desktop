//! Everything you are likely to want to change lives in this file.
//!
//! Each agent is one separate `claude` session. This file decides who the
//! agents are and what they are allowed to do; `orchestrator.rs` decides the
//! order they run in.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// --- Sign-in -----------------------------------------------------------------

/// How the agents authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Use the account you are logged in to Claude Code with. Usage counts
    /// against that plan's limits. `ANTHROPIC_API_KEY` is hidden from the
    /// agents so it cannot take over by accident.
    ClaudeLogin,
    /// Use `ANTHROPIC_API_KEY` from the environment. Billed to that API account.
    ApiKey,
}

pub const AUTH: AuthMode = AuthMode::ClaudeLogin;

// --- Models ------------------------------------------------------------------
// "opus", "sonnet" and "haiku" are aliases for the newest model of that family.
// A full model ID works here too.

pub const PLANNER_MODEL: &str = "opus";
pub const WORKER_MODEL: &str = "sonnet";
pub const REVIEWER_MODEL: &str = "sonnet";

pub const WORKER_COUNT: usize = 2;

/// Safety cap per agent session, in US dollars of API-equivalent usage.
/// `None` removes the cap.
pub const MAX_BUDGET_USD_PER_AGENT: Option<f64> = Some(5.0);

// --- Tools and permissions ---------------------------------------------------

pub const READ_TOOLS: &[&str] = &["Read", "Grep", "Glob"];
pub const EDIT_TOOLS: &[&str] = &["Edit", "Write"];
pub const SHELL_TOOL: &str = "Bash";

/// Shell commands agents may run without asking. Anything else is refused,
/// unless the project's own `.claude/settings.json` allows it. This is the
/// same permission-rule syntax Claude Code uses.
pub const CHECK_COMMANDS: &[&str] = &[
    "Bash(uv run pytest)",
    "Bash(uv run pytest *)",
    "Bash(uv run ruff check *)",
    "Bash(cargo test)",
    "Bash(cargo test *)",
    "Bash(cargo check *)",
    "Bash(cargo clippy *)",
    "Bash(git status)",
    "Bash(git diff)",
    "Bash(git diff *)",
];

// --- Role instructions -------------------------------------------------------
// Added on top of Claude Code's normal system prompt for that agent.

const PLANNER_INSTRUCTIONS: &str = "\
You are the planner in a small team: you plan, 2 workers implement, reviewers check.
You cannot edit files. Explore the project with Read, Grep and Glob, then produce a plan.

Rules for the plan:
- Split the request into at most 2 tasks. The tasks run AT THE SAME TIME in the same
  folder, so no file may appear in more than one task. If the request cannot be split
  safely, return a single task.
- Each task goes to a worker that has not seen this conversation. Write instructions that
  stand alone: what to change, the expected behaviour, and which tests to add.
- List every file the worker may create or edit in \"files\". Give each task its own test file.
- Prefer the simplest design that satisfies the request. Do not add scope.";

const WORKER_INSTRUCTIONS: &str = "\
You are a worker in a small team. You implement exactly one task from an approved plan.
Another worker is editing other files in this folder at the same time.

Rules:
- Create or edit ONLY the files listed for your task.
- Add or update tests for the behaviour you change.
- Run only your own test file while you work, not the whole suite.
- Do not commit, push, or add dependencies.
- If the task is unclear or needs a file that is not on your list, stop and say so.

Finish with a short report: what you changed (file by file), which tests you added and
their result, and anything you skipped or are unsure about.";

const REVIEWER_INSTRUCTIONS: &str = "\
You are a reviewer in a small team. You check one worker's finished task. You cannot edit files.

Steps:
1. Read every file listed for the task in full. Use `git diff` if the project is a git repo.
2. Run the tests and the linter if the project has them.
3. Check: does it do what the task asked and nothing extra? Is the new behaviour tested?
   Edge cases (empty input, None, large input)? Does it follow the project's CLAUDE.md?

Verdict \"approve\" only if there is nothing that must be fixed. Put each required fix in
\"must_fix\" as \"path/to/file:line - what is wrong\". Optional ideas go in \"consider\".";

// --- Shapes of the structured answers ----------------------------------------
// The planner and reviewers must answer with JSON matching these schemas, so
// the UI can show a plan and a verdict instead of guessing from free text.
// The Rust types below mirror the schemas field for field.

/// One unit of work for one worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub title: String,
    pub instructions: String,
    #[serde(default)]
    pub files: Vec<String>,
}

/// What the planner returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub summary: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    NeedsChanges,
}

/// What a reviewer returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub verdict: Verdict,
    pub summary: String,
    #[serde(default)]
    pub must_fix: Vec<String>,
    #[serde(default)]
    pub consider: Vec<String>,
}

fn plan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string" },
            "tasks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "instructions": { "type": "string" },
                        "files": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["title", "instructions", "files"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summary", "tasks"],
        "additionalProperties": false
    })
}

fn review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "verdict": { "type": "string", "enum": ["approve", "needs_changes"] },
            "summary": { "type": "string" },
            "must_fix": { "type": "array", "items": { "type": "string" } },
            "consider": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["verdict", "summary", "must_fix", "consider"],
        "additionalProperties": false
    })
}

// --- The agents --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Worker,
    Reviewer,
}

/// One agent: who it is, which model it runs on, and what it is allowed to do.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Stable identifier used in events, for example `worker-2`.
    pub id: String,
    pub role: Role,
    pub label: String,
    pub model: &'static str,
    pub instructions: &'static str,
    /// Tools the agent can see at all.
    pub tools: Vec<&'static str>,
    /// Tools and shell commands it may use without asking.
    pub allowed: Vec<&'static str>,
    /// When set, the agent's final answer must be JSON matching this schema.
    pub output_schema: Option<Value>,
}

pub fn planner() -> AgentSpec {
    AgentSpec {
        id: "planner".to_string(),
        role: Role::Planner,
        label: "Planner".to_string(),
        model: PLANNER_MODEL,
        instructions: PLANNER_INSTRUCTIONS,
        tools: READ_TOOLS.to_vec(),
        allowed: READ_TOOLS.to_vec(),
        output_schema: Some(plan_schema()),
    }
}

/// Worker `number`, counting from 1.
pub fn worker(number: usize) -> AgentSpec {
    AgentSpec {
        id: format!("worker-{number}"),
        role: Role::Worker,
        label: format!("Worker {number}"),
        model: WORKER_MODEL,
        instructions: WORKER_INSTRUCTIONS,
        tools: [READ_TOOLS, EDIT_TOOLS, &[SHELL_TOOL]].concat(),
        allowed: [READ_TOOLS, EDIT_TOOLS, CHECK_COMMANDS].concat(),
        output_schema: None,
    }
}

/// Reviewer `number`, which checks worker `number`.
pub fn reviewer(number: usize) -> AgentSpec {
    AgentSpec {
        id: format!("reviewer-{number}"),
        role: Role::Reviewer,
        label: format!("Reviewer {number}"),
        model: REVIEWER_MODEL,
        instructions: REVIEWER_INSTRUCTIONS,
        tools: [READ_TOOLS, &[SHELL_TOOL]].concat(),
        allowed: [READ_TOOLS, CHECK_COMMANDS].concat(),
        output_schema: Some(review_schema()),
    }
}

/// Every agent, in the order the UI lays them out.
pub fn all_agents() -> Vec<AgentSpec> {
    let mut agents = vec![planner()];
    for number in 1..=WORKER_COUNT {
        agents.push(worker(number));
        agents.push(reviewer(number));
    }
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_and_reviewers_cannot_edit() {
        for spec in all_agents() {
            let can_edit = spec.tools.contains(&"Edit") || spec.tools.contains(&"Write");
            assert_eq!(can_edit, spec.role == Role::Worker, "{}", spec.id);
        }
    }

    #[test]
    fn models_match_the_roles() {
        assert_eq!(planner().model, "opus");
        assert_eq!(worker(1).model, "sonnet");
        assert_eq!(reviewer(2).model, "sonnet");
        assert_eq!(all_agents().len(), 1 + 2 * WORKER_COUNT);
    }

    #[test]
    fn review_json_maps_onto_the_rust_type() {
        let review: Review = serde_json::from_value(json!({
            "verdict": "needs_changes", "summary": "s", "must_fix": ["a"], "consider": []
        }))
        .unwrap();
        assert_eq!(review.verdict, Verdict::NeedsChanges);
    }
}
