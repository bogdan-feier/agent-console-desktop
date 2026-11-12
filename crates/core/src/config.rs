//! Everything you are likely to want to change lives in this file.
//!
//! Each agent is one separate `claude` session. This file decides who the
//! agents are and what they are allowed to do; `orchestrator.rs` decides the
//! order they run in.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::settings::{Effort, Settings};

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

// --- Default models ------------------------------------------------------------
// The Settings window can change these; these are the starting values.
// "opus", "sonnet" and "haiku" are aliases for the newest model of that family.
// A full model ID works here too.

pub const PLANNER_MODEL: &str = "opus";
pub const WORKER_MODEL: &str = "sonnet";
pub const REVIEWER_MODEL: &str = "sonnet";

pub const WORKER_COUNT: usize = 2;

/// Default safety cap per agent session, in US dollars of API-equivalent usage.
/// `None` removes the cap. Changeable in the Settings window.
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

// --- Limits that keep usage down ------------------------------------------------

/// Most assistant turns (one reply, usually with tool calls) a session may take before
/// it stops. A stopped agent can be told to continue from its message box.
pub const PLANNER_MAX_TURNS: u32 = 30;
pub const WORKER_MAX_TURNS: u32 = 60;
pub const REVIEWER_MAX_TURNS: u32 = 20;
pub const CHAT_MAX_TURNS: u32 = 20;

/// Delay between starting two agents that run in parallel. Their prompts start the
/// same way, and a prompt-cache entry only becomes usable once the first request has
/// started answering, so the second agent can read the cached prefix instead of
/// paying to write it again.
pub const PARALLEL_START_GAP: Duration = Duration::from_secs(5);

// --- Role instructions -------------------------------------------------------
// Added on top of Claude Code's normal system prompt for that agent. Each one asks
// the agent to work economically: every file it reads and every tool result is sent
// back to the model on each following turn, so they add up.

const PLANNER_INSTRUCTIONS: &str = "\
You are the planner in a small team: you plan, up to 2 workers implement, reviewers check.
You cannot edit files. Explore the project with Read, Grep and Glob, then produce a plan.

Rules for the plan:
- Use ONE task unless the work clearly splits into two independent parts of real size.
  Every extra task starts another worker and another reviewer, which costs usage.
- With two tasks, they run AT THE SAME TIME in the same folder, so no file may appear in
  more than one task.
- Each task goes to a worker that has not seen this conversation. Write instructions that
  stand alone: what to change, the expected behaviour, and which tests to add. Name the
  functions and parts of files to change, so the worker does not have to search for them.
- List every file the worker may create or edit in \"files\". Give each task its own test file.
- Prefer the simplest design that satisfies the request. Do not add scope.

Work economically: find things with Grep and Glob before reading, read only the parts of
files you need, and do not re-read files you have already seen in this session.";

const WORKER_INSTRUCTIONS: &str = "\
You are a worker in a small team. You implement exactly one task from an approved plan.
Another worker may be editing other files in this folder at the same time.

Rules:
- Create or edit ONLY the files listed for your task.
- Add or update tests for the behaviour you change.
- Run only your own test file while you work, not the whole suite.
- Do not commit, push, or add dependencies.
- If the task is unclear or needs a file that is not on your list, stop and say so.

Work economically: find things with Grep and Glob before reading, read only the parts of
files you need, and do not re-read files you have already seen in this session.

Finish with a short report of at most 10 lines: what you changed (file by file), which tests
you added and their result, and anything you skipped or are unsure about.";

const REVIEWER_INSTRUCTIONS: &str = "\
You are a reviewer in a small team. You check one worker's finished task. You cannot edit files.

Steps:
1. Start from the changes included in the prompt. Open a file only when the changes alone
   do not give enough context. If no changes are included, read the task's files.
2. Run only the task's own test file, not the whole suite, unless the changes touch code
   that other parts of the project use.
3. Check: does it do what the task asked and nothing extra? Is the new behaviour tested?
   Edge cases (empty input, None, large input)? Does it follow the project's CLAUDE.md?

Work economically: read only what you need.

Verdict \"approve\" only if there is nothing that must be fixed. Put each required fix in
\"must_fix\" as \"path/to/file:line - what is wrong\". Optional ideas go in \"consider\".";

const PLANNER_CHAT_INSTRUCTIONS: &str = "\
You are the planner in a small team: you plan, up to 2 workers implement in parallel, reviewers
check. Right now you are brainstorming with the user, before or between plans. You cannot edit
files. Look at the project with Read, Grep and Glob when the question needs it, so the
discussion is grounded in the real code. Discuss approaches and trade-offs, point out risks,
and ask clarifying questions. Keep replies short and concrete. Do not write out a full plan:
the user presses \"Make plan\" when ready.

Work economically: search before reading, read only the parts you need, and reuse what you
already read earlier in this conversation instead of reading it again.";

const WORKER_CHAT_INSTRUCTIONS: &str = "\
You are a worker in a small team and you already worked on a task in this session.
The user is now talking to you directly. Answer their questions, or make the changes they ask for.
Stay within the files of your task unless the user names other files. Another worker may be
editing other files in this folder. Do not commit, push, or add dependencies.
If you change code, run the relevant tests and say briefly what you changed.
Reuse what you already know from this session instead of reading files again.";

const REVIEWER_CHAT_INSTRUCTIONS: &str = "\
You are a reviewer in a small team and you already reviewed a worker's task in this session.
The user is now asking you follow-up questions. Answer them briefly, using the code as evidence.
You cannot edit files. Reuse what you already read in this session instead of reading it again.";

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
    pub model: String,
    /// `None` leaves the effort to Claude Code's default.
    pub effort: Option<Effort>,
    pub instructions: &'static str,
    /// Tools the agent can see at all.
    pub tools: Vec<&'static str>,
    /// Tools and shell commands it may use without asking.
    pub allowed: Vec<&'static str>,
    /// When set, the agent's final answer must be JSON matching this schema.
    pub output_schema: Option<Value>,
    /// Spending cap for this session, in US dollars.
    pub max_budget_usd: Option<f64>,
    /// Most assistant turns this session may take.
    pub max_turns: u32,
    /// Whether the session loads the MCP servers configured in Claude Code.
    pub use_mcp_servers: bool,
    /// True when the user is talking to the agent directly, rather than giving it
    /// a task from the plan.
    pub chat: bool,
}

/// The planner when it turns the request (or the conversation so far) into a plan.
pub fn planner(settings: &Settings) -> AgentSpec {
    AgentSpec {
        id: "planner".to_string(),
        role: Role::Planner,
        label: "Planner".to_string(),
        model: settings.planner.model.clone(),
        effort: settings.planner.effort,
        instructions: PLANNER_INSTRUCTIONS,
        tools: READ_TOOLS.to_vec(),
        allowed: READ_TOOLS.to_vec(),
        output_schema: Some(plan_schema()),
        max_budget_usd: settings.max_budget_usd,
        max_turns: PLANNER_MAX_TURNS,
        use_mcp_servers: settings.use_mcp_servers,
        chat: false,
    }
}

/// The planner while brainstorming with the user: same tools, free-form answers.
pub fn planner_chat(settings: &Settings) -> AgentSpec {
    AgentSpec {
        instructions: PLANNER_CHAT_INSTRUCTIONS,
        output_schema: None,
        max_turns: CHAT_MAX_TURNS,
        chat: true,
        ..planner(settings)
    }
}

/// Worker `number`, counting from 1.
pub fn worker(settings: &Settings, number: usize) -> AgentSpec {
    AgentSpec {
        id: format!("worker-{number}"),
        role: Role::Worker,
        label: format!("Worker {number}"),
        model: settings.worker.model.clone(),
        effort: settings.worker.effort,
        instructions: WORKER_INSTRUCTIONS,
        tools: [READ_TOOLS, EDIT_TOOLS, &[SHELL_TOOL]].concat(),
        allowed: [READ_TOOLS, EDIT_TOOLS, CHECK_COMMANDS].concat(),
        output_schema: None,
        max_budget_usd: settings.max_budget_usd,
        max_turns: WORKER_MAX_TURNS,
        use_mcp_servers: settings.use_mcp_servers,
        chat: false,
    }
}

/// Worker `number` when the user talks to it directly.
pub fn worker_chat(settings: &Settings, number: usize) -> AgentSpec {
    AgentSpec {
        instructions: WORKER_CHAT_INSTRUCTIONS,
        max_turns: CHAT_MAX_TURNS,
        chat: true,
        ..worker(settings, number)
    }
}

/// Reviewer `number`, which checks worker `number`.
pub fn reviewer(settings: &Settings, number: usize) -> AgentSpec {
    AgentSpec {
        id: format!("reviewer-{number}"),
        role: Role::Reviewer,
        label: format!("Reviewer {number}"),
        model: settings.reviewer.model.clone(),
        effort: settings.reviewer.effort,
        instructions: REVIEWER_INSTRUCTIONS,
        tools: [READ_TOOLS, &[SHELL_TOOL]].concat(),
        allowed: [READ_TOOLS, CHECK_COMMANDS].concat(),
        output_schema: Some(review_schema()),
        max_budget_usd: settings.max_budget_usd,
        max_turns: REVIEWER_MAX_TURNS,
        use_mcp_servers: settings.use_mcp_servers,
        chat: false,
    }
}

/// Reviewer `number` when the user asks it follow-up questions: free-form answers.
pub fn reviewer_chat(settings: &Settings, number: usize) -> AgentSpec {
    AgentSpec {
        instructions: REVIEWER_CHAT_INSTRUCTIONS,
        output_schema: None,
        max_turns: CHAT_MAX_TURNS,
        chat: true,
        ..reviewer(settings, number)
    }
}

/// The chat version of the agent with this ID, or `None` if there is no such agent.
pub fn chat_spec(settings: &Settings, agent_id: &str) -> Option<AgentSpec> {
    if agent_id == "planner" {
        return Some(planner_chat(settings));
    }
    let (role, number) = agent_id.split_once('-')?;
    let number: usize = number.parse().ok().filter(|n| (1..=WORKER_COUNT).contains(n))?;
    match role {
        "worker" => Some(worker_chat(settings, number)),
        "reviewer" => Some(reviewer_chat(settings, number)),
        _ => None,
    }
}

/// Every agent, in the order the UI lays them out.
pub fn all_agents(settings: &Settings) -> Vec<AgentSpec> {
    let mut agents = vec![planner(settings)];
    for number in 1..=WORKER_COUNT {
        agents.push(worker(settings, number));
        agents.push(reviewer(settings, number));
    }
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Settings {
        Settings::default()
    }

    #[test]
    fn planner_and_reviewers_cannot_edit() {
        for spec in all_agents(&defaults()) {
            let can_edit = spec.tools.contains(&"Edit") || spec.tools.contains(&"Write");
            assert_eq!(can_edit, spec.role == Role::Worker, "{}", spec.id);
        }
    }

    #[test]
    fn chats_have_a_lower_turn_limit_than_tasks() {
        let settings = defaults();
        assert_eq!(worker(&settings, 1).max_turns, WORKER_MAX_TURNS);
        assert_eq!(worker_chat(&settings, 1).max_turns, CHAT_MAX_TURNS);
        assert_eq!(planner_chat(&settings).max_turns, CHAT_MAX_TURNS);
        assert!(!planner(&settings).use_mcp_servers, "MCP servers are off by default");
    }

    #[test]
    fn models_and_effort_come_from_the_settings() {
        let mut settings = defaults();
        settings.planner.effort = Some(Effort::Max);
        settings.reviewer.model = "haiku".to_string();
        assert_eq!(planner(&settings).model, "opus");
        assert_eq!(planner(&settings).effort, Some(Effort::Max));
        assert_eq!(worker(&settings, 1).model, "sonnet");
        assert_eq!(reviewer(&settings, 2).model, "haiku");
        assert_eq!(all_agents(&settings).len(), 1 + 2 * WORKER_COUNT);
    }

    #[test]
    fn chat_versions_answer_freely_but_keep_their_tools() {
        let settings = defaults();
        let planner_chat = chat_spec(&settings, "planner").unwrap();
        assert!(planner_chat.chat && planner_chat.output_schema.is_none());
        assert_eq!(planner_chat.tools, planner(&settings).tools);

        let reviewer_chat = chat_spec(&settings, "reviewer-2").unwrap();
        assert!(reviewer_chat.output_schema.is_none());
        assert!(!reviewer_chat.tools.contains(&"Edit"));

        assert_eq!(chat_spec(&settings, "worker-1").unwrap().id, "worker-1");
        for unknown in ["worker-0", "worker-3", "reviewer-x", "boss", "worker"] {
            assert!(chat_spec(&settings, unknown).is_none(), "{unknown}");
        }
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
