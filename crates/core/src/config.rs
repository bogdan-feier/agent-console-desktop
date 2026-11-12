//! Everything you are likely to want to change lives in this file.
//!
//! Each agent is one separate `claude` session. This file decides who the
//! agents are and what they are allowed to do; `orchestrator.rs` decides the
//! order they run in.
//!
//! The team, for one project:
//!
//! ```text
//! you -> Jarvis (Haiku) -> answers quick questions itself, or passes the message on
//!                          to the planner, to brainstorm or to plan
//! planner (Opus) -> you approve -> up to 3 workers (Sonnet), each in its own git worktree
//!                -> one reviewer sees all their changes merged -> you apply or discard
//! ```

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

pub const JARVIS_MODEL: &str = "haiku";
pub const PLANNER_MODEL: &str = "opus";
pub const WORKER_MODEL: &str = "sonnet";
pub const REVIEWER_MODEL: &str = "sonnet";

/// Most workers a plan can use. The planner uses fewer when the work does not split.
pub const WORKER_COUNT: usize = 3;

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
pub const JARVIS_MAX_TURNS: u32 = 8;
pub const PLANNER_MAX_TURNS: u32 = 30;
pub const WORKER_MAX_TURNS: u32 = 60;
pub const REVIEWER_MAX_TURNS: u32 = 30;
pub const CHAT_MAX_TURNS: u32 = 20;

/// Delay between starting two agents that run in parallel. Their prompts start the
/// same way, and a prompt-cache entry only becomes usable once the first request has
/// started answering, so the later agents can read the cached prefix instead of
/// paying to write it again.
pub const PARALLEL_START_GAP: Duration = Duration::from_secs(5);

// --- Role instructions -------------------------------------------------------
// Added on top of Claude Code's normal system prompt for that agent. Each one asks
// the agent to work economically: every file it reads and every tool result is sent
// back to the model on each following turn, so they add up.

const JARVIS_INSTRUCTIONS: &str = "\
You are Jarvis, the front desk of a small team working on this one project: a planner
(brainstorms with the user and writes plans), up to 3 workers (write the code) and a reviewer.
You cannot edit files. For each message from the user, choose one route:

- \"answer\": a quick question you can answer yourself after at most a few Grep, Glob or Read
  calls (what does this function do, where is that handled, how are the tests run).
  Put the full answer in \"reply\".
- \"brainstorm\": the user wants to discuss an idea, a design or a trade-off, or the request
  is still vague. Put one short sentence in \"reply\" saying you passed it to the planner.
- \"plan\": the user clearly asks for a change and it is specific enough to plan.
  Put one short sentence in \"reply\" saying you passed it to the planner.

If you are unsure between brainstorm and plan, choose brainstorm. You are the cheap first step:
use as few tool calls as possible, and do not explore the project for questions you route on.";

const PLANNER_INSTRUCTIONS: &str = "\
You are the planner in a small team: you plan, up to 3 workers implement, a reviewer checks.
You cannot edit files. Explore the project with Read, Grep and Glob, then produce a plan.

Rules for the plan:
- Use ONE task unless the work clearly splits into independent parts of real size. Use at
  most 3. Every extra task starts another worker, which costs usage.
- Each worker gets its own copy of the project (a git worktree) and their work is merged
  afterwards, so two tasks must never change the same file: that makes the merge fail.
- Each task goes to a worker that has not seen this conversation. Write instructions that
  stand alone: what to change, the expected behaviour, and which tests to add. Name the
  functions and parts of files to change, so the worker does not have to search for them.
- List every file the worker may create or edit in \"files\". Give each task its own test file.
- Prefer the simplest design that satisfies the request. Do not add scope.

Work economically: find things with Grep and Glob before reading, read only the parts of
files you need, and do not re-read files you have already seen in this session.";

const WORKER_INSTRUCTIONS: &str = "\
You are a worker in a small team. You implement exactly one task from an approved plan, in
your own copy of the project. Other workers do other tasks in their own copies at the same time,
and all the work is merged afterwards.

Rules:
- Create or edit ONLY the files listed for your task; anything else may clash in the merge.
- Add or update tests for the behaviour you change.
- Run only your own test file while you work, not the whole suite.
- Do not commit, push, or add dependencies. The app commits your work when you finish.
- If the task is unclear or needs a file that is not on your list, stop and say so.

Work economically: find things with Grep and Glob before reading, read only the parts of
files you need, and do not re-read files you have already seen in this session.

Finish with a short report of at most 10 lines: what you changed (file by file), which tests
you added and their result, and anything you skipped or are unsure about.";

const REVIEWER_INSTRUCTIONS: &str = "\
You are the reviewer in a small team. Up to 3 workers each did one task of a plan; their work is
merged in the folder you are in. You review all of it together. You cannot edit files.

Steps:
1. Start from the changes included in the prompt. Open a file only when the changes alone
   do not give enough context.
2. Run the project's tests once, so you see how the tasks work together.
3. Check each task: does it do what was asked and nothing extra? Is the new behaviour tested?
   Edge cases (empty input, None, large input)? Does it follow the project's CLAUDE.md?
   Also check that the tasks fit together.

Work economically: read only what you need, and run the tests once unless something changed.

Verdict \"approve\" only if there is nothing that must be fixed. Put each required fix in
\"must_fix\" with the number of the task whose worker should fix it, and the problem as
\"path/to/file:line - what is wrong\". Optional ideas go in \"consider\".";

const PLANNER_CHAT_INSTRUCTIONS: &str = "\
You are the planner in a small team: you plan, up to 3 workers implement in parallel, a reviewer
checks. Right now you are brainstorming with the user, before or between plans. You cannot edit
files. Look at the project with Read, Grep and Glob when the question needs it, so the
discussion is grounded in the real code. Discuss approaches and trade-offs, point out risks,
and ask clarifying questions. Keep replies short and concrete. Do not write out a full plan:
the user presses \"Make plan\" when ready.

Work economically: search before reading, read only the parts you need, and reuse what you
already read earlier in this conversation instead of reading it again.";

const WORKER_CHAT_INSTRUCTIONS: &str = "\
You are a worker in a small team and you already worked on a task in this session, in your own
copy of the project. The user is now talking to you directly. Answer their questions, or make
the changes they ask for. Stay within the files of your task unless the user names other files.
Do not commit, push, or add dependencies. If you change code, run the relevant tests and say
briefly what you changed. Reuse what you already know from this session instead of reading
files again.";

const REVIEWER_CHAT_INSTRUCTIONS: &str = "\
You are the reviewer in a small team and you already reviewed the workers' tasks in this
session. The user is now asking you follow-up questions. Answer them briefly, using the code as
evidence. You cannot edit files. Reuse what you already read in this session instead of reading
it again.";

// --- Shapes of the structured answers ----------------------------------------
// Jarvis, the planner and the reviewer must answer with JSON matching these
// schemas, so the app can act on the answer instead of guessing from free text.
// The Rust types below mirror the schemas field for field.

/// Where Jarvis sends a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Jarvis answered it itself.
    Answer,
    /// Pass it to the planner to discuss.
    Brainstorm,
    /// Pass it to the planner to make a plan.
    Plan,
}

/// What Jarvis returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routing {
    pub route: Route,
    pub reply: String,
}

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

/// One problem the reviewer wants fixed, and which task's worker should fix it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Task number, counting from 1, which is also the worker's number.
    pub task: usize,
    pub issue: String,
}

/// What the reviewer returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub verdict: Verdict,
    pub summary: String,
    #[serde(default)]
    pub must_fix: Vec<Finding>,
    #[serde(default)]
    pub consider: Vec<String>,
}

impl Review {
    /// The problems worker `number` should fix.
    pub fn findings_for(&self, number: usize) -> Vec<&Finding> {
        self.must_fix.iter().filter(|finding| finding.task == number).collect()
    }
}

fn routing_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "route": { "type": "string", "enum": ["answer", "brainstorm", "plan"] },
            "reply": { "type": "string" }
        },
        "required": ["route", "reply"],
        "additionalProperties": false
    })
}

fn plan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string" },
            "tasks": {
                "type": "array",
                "maxItems": WORKER_COUNT,
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
            "must_fix": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "integer", "minimum": 1, "maximum": WORKER_COUNT },
                        "issue": { "type": "string" }
                    },
                    "required": ["task", "issue"],
                    "additionalProperties": false
                }
            },
            "consider": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["verdict", "summary", "must_fix", "consider"],
        "additionalProperties": false
    })
}

// --- The agents --------------------------------------------------------------

pub const JARVIS_ID: &str = "jarvis";
pub const PLANNER_ID: &str = "planner";
pub const REVIEWER_ID: &str = "reviewer";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Jarvis,
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

/// Jarvis, the cheap front desk: answers quick questions or routes the message on.
pub fn jarvis(settings: &Settings) -> AgentSpec {
    AgentSpec {
        id: JARVIS_ID.to_string(),
        role: Role::Jarvis,
        label: "Jarvis".to_string(),
        model: settings.jarvis.model.clone(),
        effort: settings.jarvis.effort,
        instructions: JARVIS_INSTRUCTIONS,
        tools: READ_TOOLS.to_vec(),
        allowed: READ_TOOLS.to_vec(),
        output_schema: Some(routing_schema()),
        max_budget_usd: settings.max_budget_usd,
        max_turns: JARVIS_MAX_TURNS,
        use_mcp_servers: settings.use_mcp_servers,
        chat: true,
    }
}

/// The planner when it turns the request (or the conversation so far) into a plan.
pub fn planner(settings: &Settings) -> AgentSpec {
    AgentSpec {
        id: PLANNER_ID.to_string(),
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

/// The reviewer, which checks all the workers' changes together.
pub fn reviewer(settings: &Settings) -> AgentSpec {
    AgentSpec {
        id: REVIEWER_ID.to_string(),
        role: Role::Reviewer,
        label: "Reviewer".to_string(),
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

/// The reviewer when the user asks it follow-up questions: free-form answers.
pub fn reviewer_chat(settings: &Settings) -> AgentSpec {
    AgentSpec {
        instructions: REVIEWER_CHAT_INSTRUCTIONS,
        output_schema: None,
        max_turns: CHAT_MAX_TURNS,
        chat: true,
        ..reviewer(settings)
    }
}

/// The chat version of the agent with this ID, or `None` if you cannot message it
/// directly (Jarvis is reached through the main message box instead).
pub fn chat_spec(settings: &Settings, agent_id: &str) -> Option<AgentSpec> {
    match agent_id {
        PLANNER_ID => Some(planner_chat(settings)),
        REVIEWER_ID => Some(reviewer_chat(settings)),
        _ => worker_number(agent_id).map(|number| worker_chat(settings, number)),
    }
}

/// "worker-2" -> Some(2), for numbers from 1 to `WORKER_COUNT`; anything else -> None.
pub fn worker_number(agent_id: &str) -> Option<usize> {
    let number: usize = agent_id.strip_prefix("worker-")?.parse().ok()?;
    (1..=WORKER_COUNT).contains(&number).then_some(number)
}

/// Every agent, in the order the page lays them out.
pub fn all_agents(settings: &Settings) -> Vec<AgentSpec> {
    let mut agents = vec![jarvis(settings), planner(settings)];
    agents.extend((1..=WORKER_COUNT).map(|number| worker(settings, number)));
    agents.push(reviewer(settings));
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Settings {
        Settings::default()
    }

    #[test]
    fn only_workers_can_edit() {
        for spec in all_agents(&defaults()) {
            let can_edit = spec.tools.contains(&"Edit") || spec.tools.contains(&"Write");
            assert_eq!(can_edit, spec.role == Role::Worker, "{}", spec.id);
        }
    }

    #[test]
    fn the_team_is_jarvis_planner_three_workers_and_one_reviewer() {
        let ids: Vec<String> = all_agents(&defaults()).into_iter().map(|spec| spec.id).collect();
        assert_eq!(ids, ["jarvis", "planner", "worker-1", "worker-2", "worker-3", "reviewer"]);
    }

    #[test]
    fn chats_have_a_lower_turn_limit_than_tasks() {
        let settings = defaults();
        assert_eq!(worker(&settings, 1).max_turns, WORKER_MAX_TURNS);
        assert_eq!(worker_chat(&settings, 1).max_turns, CHAT_MAX_TURNS);
        assert_eq!(planner_chat(&settings).max_turns, CHAT_MAX_TURNS);
        assert!(jarvis(&settings).max_turns < PLANNER_MAX_TURNS);
        assert!(!planner(&settings).use_mcp_servers, "MCP servers are off by default");
    }

    #[test]
    fn models_and_effort_come_from_the_settings() {
        let mut settings = defaults();
        settings.planner.effort = Some(Effort::Max);
        settings.reviewer.model = "haiku".to_string();
        assert_eq!(jarvis(&settings).model, "haiku");
        assert_eq!(planner(&settings).model, "opus");
        assert_eq!(planner(&settings).effort, Some(Effort::Max));
        assert_eq!(worker(&settings, 3).model, "sonnet");
        assert_eq!(reviewer(&settings).model, "haiku");
    }

    #[test]
    fn chat_versions_answer_freely_but_keep_their_tools() {
        let settings = defaults();
        let planner_chat = chat_spec(&settings, "planner").unwrap();
        assert!(planner_chat.chat && planner_chat.output_schema.is_none());
        assert_eq!(planner_chat.tools, planner(&settings).tools);

        let reviewer_chat = chat_spec(&settings, "reviewer").unwrap();
        assert!(reviewer_chat.output_schema.is_none());
        assert!(!reviewer_chat.tools.contains(&"Edit"));

        assert_eq!(chat_spec(&settings, "worker-3").unwrap().id, "worker-3");
        for not_messageable in ["jarvis", "worker-0", "worker-4", "reviewer-1", "boss", "worker"] {
            assert!(chat_spec(&settings, not_messageable).is_none(), "{not_messageable}");
        }
    }

    #[test]
    fn review_findings_are_grouped_by_task() {
        let review: Review = serde_json::from_value(json!({
            "verdict": "needs_changes", "summary": "s", "consider": [],
            "must_fix": [{"task": 2, "issue": "a"}, {"task": 1, "issue": "b"}, {"task": 2, "issue": "c"}]
        }))
        .unwrap();
        assert_eq!(review.verdict, Verdict::NeedsChanges);
        let for_two: Vec<&str> = review.findings_for(2).iter().map(|f| f.issue.as_str()).collect();
        assert_eq!(for_two, ["a", "c"]);
        assert!(review.findings_for(3).is_empty());
    }

    #[test]
    fn routing_json_maps_onto_the_rust_type() {
        let routing: Routing =
            serde_json::from_value(json!({"route": "brainstorm", "reply": "Passed on."})).unwrap();
        assert_eq!(routing.route, Route::Brainstorm);
    }
}
