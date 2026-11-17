//! Everything you are likely to want to change lives in this file.
//!
//! Each agent is one separate `claude` session. This file decides who the
//! agents are and what they are allowed to do; `orchestrator.rs` decides the
//! order they run in.
//!
//! The team, for one project:
//!
//! ```text
//! you -> dispatcher (Haiku) -> answers quick questions itself, or passes the message on
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

pub const DISPATCHER_MODEL: &str = "haiku";
pub const PLANNER_MODEL: &str = "opus";
pub const WORKER_MODEL: &str = "sonnet";
pub const REVIEWER_MODEL: &str = "sonnet";

/// Most workers a plan can ever use (the Settings window can lower it). The planner
/// uses fewer when the work does not split.
pub const WORKER_COUNT: usize = 3;

/// Default safety cap per agent session, in US dollars of API-equivalent usage.
/// `None` removes the cap. Changeable in the Settings window.
pub const MAX_BUDGET_USD_PER_AGENT: Option<f64> = Some(5.0);

// --- Tools and permissions ---------------------------------------------------

pub const READ_TOOLS: &[&str] = &["Read", "Grep", "Glob"];
pub const EDIT_TOOLS: &[&str] = &["Edit", "Write"];
pub const SHELL_TOOL: &str = "Bash";
/// Given to the dispatcher and the planner when "web access" is on in Settings.
pub const WEB_TOOLS: &[&str] = &["WebSearch", "WebFetch"];

/// The default shell commands agents may run without asking (the Settings window
/// can change the list). Anything else is refused, unless the project's own
/// `.claude/settings.json` allows it. This is the same permission-rule syntax
/// Claude Code uses.
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

/// Never allowed, however wide the allow list is. Deny beats allow in Claude Code,
/// so these hold even with "any command" switched on.
///
/// The agents work in copies of the project, but they run as you, on your machine,
/// with your keys and your sign-ins. This list is about the things that reach past
/// their copy: your real repository, your account, your machine, your secrets.
/// Argument patterns are not airtight (`rm -fr` is not `rm -rf`), so treat this as
/// a seatbelt, not a wall: it stops the careless case, not a determined one.
pub const DENIED_COMMANDS: &[&str] = &[
    "Bash(sudo *)",
    "Bash(rm -rf *)",
    "Bash(rm -fr *)",
    "Bash(rm -r *)",
    // Your repository and your account, as opposed to the worker's own copy.
    "Bash(git push *)",
    "Bash(git config *)",
    "Bash(git remote *)",
    "Bash(gh *)",
    // Your secrets: the macOS keychain, and anything that reaches the network and
    // could carry them out of the machine.
    "Bash(security *)",
    "Bash(curl *)",
    "Bash(wget *)",
    "Bash(ssh *)",
    "Bash(scp *)",
    // Your machine's own settings and running programs.
    "Bash(launchctl *)",
    "Bash(defaults *)",
    "Bash(killall *)",
    "Bash(shutdown *)",
];

// --- Limits that keep usage down ------------------------------------------------

/// Default for the most assistant turns (one reply, usually with tool calls) a session
/// may take before it stops; the Settings window can change each one. A stopped agent
/// can be told to continue from its message box.
pub const DISPATCHER_MAX_TURNS: u32 = 8;
pub const PLANNER_MAX_TURNS: u32 = 30;
pub const WORKER_MAX_TURNS: u32 = 60;
pub const REVIEWER_MAX_TURNS: u32 = 30;
/// The resolver only has to make two versions of a few files fit together.
pub const RESOLVER_MAX_TURNS: u32 = 20;
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

const DISPATCHER_INSTRUCTIONS: &str = "\
You are the dispatcher, the front desk of a small team working on this one project: a planner
(brainstorms with the user and writes plans), workers (write the code) and a reviewer.
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
You are the planner in a small team: you plan, up to {max_tasks} worker(s) implement, a reviewer
checks. You cannot edit files. Explore the project with Read, Grep and Glob, then produce a plan.

Rules for the plan:
- Default to ONE task. Use at most {max_tasks}, and only split when each part would be
  real work on its own: a separate file, a separate concern, half an hour of work each.
  Adding a function and the flag that calls it is ONE task. Adding a function and its
  test is ONE task. Splitting work a single worker would do in one pass costs a whole
  extra agent, and every agent pays for its own system prompt before it does anything.
  If you are unsure whether to split, do not.
- Never plan a task that changes dependencies or lock files (pyproject.toml, package.json,
  Cargo.toml, uv.lock and the like). Those belong to the user, between rounds. Say so in
  your summary instead, so they can do it.
- Each worker gets its own copy of the project (a git worktree) and their work is merged
  afterwards, so two tasks must never change the same file: that makes the merge fail.
- Each task goes to a worker that has not seen this conversation. Write instructions that
  stand alone: what to change, the expected behaviour, and which tests to add. Name the
  functions and parts of files to change, so the worker does not have to search for them.
- List every file the worker may create or edit in \"files\". Give each task its own test file.
- Put in \"context\" what you found while reading that this worker will need, so it does
  not have to go and find it again: the signatures it will call or change, where in each
  file they are, how the project writes its tests, anything about the code that would
  surprise it. Write only what you actually read, and say where you read it. Do not
  guess, do not paste whole files, and do not repeat the instructions.
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
You are the reviewer in a small team. One or more workers each did one task of a plan; their work is
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
\"path/to/file:line - what is wrong\". Optional ideas go in \"consider\".

Each entry in \"must_fix\" needs the number of the task whose worker will fix it, and that
worker may only edit the files listed for its own task. If what has to change is in a file
no task lists, no worker can do it: set \"task\" to 0 and say in the issue which file and
what to change, and the user will do it themselves. Never give a finding to a task whose
file list does not cover it; that worker will refuse it and the round gets stuck.

In \"commit_message\", write the subject line for a commit of all this work: one line, at
most 72 characters, imperative mood, describing what the change does rather than listing
the tasks. Match the style of the project's recent commit subjects, shown in the prompt:
if they carry a prefix such as \"feat:\" or \"fix:\", use one. Write it as the author of
the change; do not mention the review, this app, or who or what wrote the code.";

/// The resolver runs inside a merge that git could not finish. Its whole job is to
/// make the clashing files whole again without losing either worker's work.
const RESOLVER_INSTRUCTIONS: &str = "\
You are the resolver on a small team. Two workers changed the same lines of a file, so git \
could not merge their work. The files you are given are in your folder with git's conflict \
markers in them: <<<<<<< for the first side, ======= between the two, >>>>>>> after the second.

Your only job is to edit those files so that the markers are gone and both workers' work is \
still there.

Rules:
- Touch only the files you are given. Nothing else, however tempting.
- Keep both sides. If one side renamed something and the other used the old name, make them \
  agree instead of dropping either change.
- Never delete one side's work just to make the conflict go away.
- Do not improve, tidy or add anything beyond making the two versions fit together.
- Leave no conflict marker anywhere, in any file.
- If the two changes truly cannot both stand, keep as much of both as you can and say so \
  plainly in your report.

Then report, in two or three sentences: what each side was doing, and what you did about it. \
The reviewer reads the result afterwards, and the user sees every line before it reaches \
their project.";

const PLANNER_CHAT_INSTRUCTIONS: &str = "\
You are the planner in a small team: you plan, up to {max_tasks} worker(s) implement in parallel,
a reviewer checks. Right now you are brainstorming with the user, before or between plans. You cannot edit
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
// The dispatcher, the planner and the reviewer must answer with JSON matching these
// schemas, so the app can act on the answer instead of guessing from free text.
// The Rust types below mirror the schemas field for field.

/// Where the dispatcher sends a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The dispatcher answered it itself.
    Answer,
    /// Pass it to the planner to discuss.
    Brainstorm,
    /// Pass it to the planner to make a plan.
    Plan,
}

/// What the dispatcher returns.
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
    /// What the planner already found out that this task's worker would otherwise
    /// have to go and find for itself: where the code lives, what the signatures
    /// are, how the project does things. The planner has just read the project; the
    /// worker starts with none of that, and without this it reads the same files
    /// again. One read instead of one per worker.
    ///
    /// `#[serde(default)]` so plans saved before this field existed still load.
    #[serde(default)]
    pub context: String,
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
    ///
    /// `0` means no task owns the file this is about, so no worker is allowed to
    /// touch it and sending it back to one would only get it refused. Those are the
    /// user's to fix. See [`Review::for_the_user`].
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
    /// A commit message for the round, written by the reviewer: it has just read
    /// every change together, which is exactly what a commit message describes.
    /// `#[serde(default)]` so reviews saved before this field existed still load.
    #[serde(default)]
    pub commit_message: String,
}

impl Review {
    /// The problems worker `number` should fix.
    pub fn findings_for(&self, number: usize) -> Vec<&Finding> {
        self.must_fix.iter().filter(|finding| finding.task == number).collect()
    }

    /// Findings no worker is allowed to fix, because the file belongs to no task.
    pub fn for_the_user(&self) -> Vec<&Finding> {
        self.findings_for(0)
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

fn plan_schema(max_tasks: usize) -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string" },
            "tasks": {
                "type": "array",
                "maxItems": max_tasks,
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "instructions": { "type": "string" },
                        "files": { "type": "array", "items": { "type": "string" } },
                        "context": { "type": "string" }
                    },
                    "required": ["title", "instructions", "files", "context"],
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
                        "task": { "type": "integer", "minimum": 0, "maximum": WORKER_COUNT },
                        "issue": { "type": "string" }
                    },
                    "required": ["task", "issue"],
                    "additionalProperties": false
                }
            },
            "consider": { "type": "array", "items": { "type": "string" } },
            "commit_message": { "type": "string" }
        },
        "required": ["verdict", "summary", "must_fix", "consider", "commit_message"],
        "additionalProperties": false
    })
}

// --- The agents --------------------------------------------------------------

pub const DISPATCHER_ID: &str = "dispatcher";
pub const PLANNER_ID: &str = "planner";
pub const REVIEWER_ID: &str = "reviewer";
/// The only worker a quick change uses.
pub const WORKER_ONE: &str = "worker-1";
pub const RESOLVER_ID: &str = "resolver";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Dispatcher,
    Planner,
    Worker,
    Reviewer,
    /// Merges two workers' changes when they clash. Only runs when that happens.
    Resolver,
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
    /// The role's instructions, plus the user's notes from Settings.
    pub instructions: String,
    /// Tools the agent can see at all.
    pub tools: Vec<String>,
    /// Tools and shell commands it may use without asking.
    pub allowed: Vec<String>,
    /// Commands it may never run, whatever `allowed` says. Deny wins over allow.
    pub denied: Vec<String>,
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

// --- Building blocks for the specs below ---

/// `["Read", "Grep"]` -> owned strings, so lists from constants and from the settings mix.
fn names(lists: &[&[&str]]) -> Vec<String> {
    lists.concat().into_iter().map(String::from).collect()
}

/// The role's instructions with the user's notes from Settings added at the end.
/// `{max_tasks}` in the text becomes the number of workers a plan may use.
fn instructions(role: &str, settings: &Settings) -> String {
    let mut text = role.replace("{max_tasks}", &settings.worker_count.to_string());
    if !settings.notes.is_empty() {
        text.push_str("\n\nNotes from the user for every agent on this project:\n");
        text.push_str(&settings.notes);
    }
    text
}

/// Read-only tools, plus web search when Settings allows it.
fn reading_tools(settings: &Settings) -> Vec<String> {
    let web: &[&str] = if settings.web_access { WEB_TOOLS } else { &[] };
    names(&[READ_TOOLS, web])
}

/// The allowed commands from Settings as permission rules: "cargo test" -> "Bash(cargo test)".
fn command_rules(settings: &Settings) -> Vec<String> {
    settings.allowed_commands.iter().map(|command| format!("{SHELL_TOOL}({command})")).collect()
}

/// What the agent can see, what it may run without asking, and what it may never run.
///
/// With "any command" on, the shell is allowed outright and only [`DENIED_COMMANDS`]
/// holds it back. Otherwise only the commands from Settings are allowed, and with
/// none listed the shell is left out entirely, which also saves its description on
/// every turn.
fn shell_access(settings: &Settings, extra: &[&str]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut tools = names(&[READ_TOOLS, extra]);
    let mut allowed = tools.clone();
    let mut denied = Vec::new();

    if settings.full_shell {
        tools.push(SHELL_TOOL.to_string());
        allowed.push(format!("{SHELL_TOOL}(*)"));
        denied = names(&[DENIED_COMMANDS]);
    } else {
        let rules = command_rules(settings);
        if !rules.is_empty() {
            tools.push(SHELL_TOOL.to_string());
            allowed.extend(rules);
            denied = names(&[DENIED_COMMANDS]);
        }
    }
    (tools, allowed, denied)
}

fn chat_turns(settings: &Settings) -> u32 {
    settings.chat_max_turns.unwrap_or(CHAT_MAX_TURNS)
}

/// The dispatcher, the cheap front desk: answers quick questions or routes the message on.
pub fn dispatcher(settings: &Settings) -> AgentSpec {
    AgentSpec {
        id: DISPATCHER_ID.to_string(),
        role: Role::Dispatcher,
        label: "Dispatcher".to_string(),
        model: settings.dispatcher.model.clone(),
        effort: settings.dispatcher.effort,
        instructions: instructions(DISPATCHER_INSTRUCTIONS, settings),
        tools: reading_tools(settings),
        allowed: reading_tools(settings),
        denied: Vec::new(),
        output_schema: Some(routing_schema()),
        max_budget_usd: settings.max_budget_usd,
        max_turns: settings.dispatcher.max_turns.unwrap_or(DISPATCHER_MAX_TURNS),
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
        instructions: instructions(PLANNER_INSTRUCTIONS, settings),
        tools: reading_tools(settings),
        allowed: reading_tools(settings),
        denied: Vec::new(),
        output_schema: Some(plan_schema(settings.worker_count)),
        max_budget_usd: settings.max_budget_usd,
        max_turns: settings.planner.max_turns.unwrap_or(PLANNER_MAX_TURNS),
        use_mcp_servers: settings.use_mcp_servers,
        chat: false,
    }
}

/// The planner while brainstorming with the user: same tools, free-form answers.
pub fn planner_chat(settings: &Settings) -> AgentSpec {
    AgentSpec {
        instructions: instructions(PLANNER_CHAT_INSTRUCTIONS, settings),
        output_schema: None,
        max_turns: chat_turns(settings),
        chat: true,
        ..planner(settings)
    }
}

/// Worker `number`, counting from 1.
pub fn worker(settings: &Settings, number: usize) -> AgentSpec {
    let (tools, allowed, denied) = shell_access(settings, EDIT_TOOLS);
    AgentSpec {
        id: format!("worker-{number}"),
        role: Role::Worker,
        label: format!("Worker {number}"),
        model: settings.worker.model.clone(),
        effort: settings.worker.effort,
        instructions: instructions(WORKER_INSTRUCTIONS, settings),
        tools,
        allowed,
        denied,
        output_schema: None,
        max_budget_usd: settings.max_budget_usd,
        max_turns: settings.worker.max_turns.unwrap_or(WORKER_MAX_TURNS),
        use_mcp_servers: settings.use_mcp_servers,
        chat: false,
    }
}

/// Worker `number` when the user talks to it directly.
pub fn worker_chat(settings: &Settings, number: usize) -> AgentSpec {
    AgentSpec {
        instructions: instructions(WORKER_CHAT_INSTRUCTIONS, settings),
        max_turns: chat_turns(settings),
        chat: true,
        ..worker(settings, number)
    }
}

/// The reviewer, which checks all the workers' changes together.
pub fn reviewer(settings: &Settings) -> AgentSpec {
    let (tools, allowed, denied) = shell_access(settings, &[]);
    AgentSpec {
        id: REVIEWER_ID.to_string(),
        role: Role::Reviewer,
        label: "Reviewer".to_string(),
        model: settings.reviewer.model.clone(),
        effort: settings.reviewer.effort,
        instructions: instructions(REVIEWER_INSTRUCTIONS, settings),
        tools,
        allowed,
        denied,
        output_schema: Some(review_schema()),
        max_budget_usd: settings.max_budget_usd,
        max_turns: settings.reviewer.max_turns.unwrap_or(REVIEWER_MAX_TURNS),
        use_mcp_servers: settings.use_mcp_servers,
        chat: false,
    }
}

/// The reviewer when the user asks it follow-up questions: free-form answers.
pub fn reviewer_chat(settings: &Settings) -> AgentSpec {
    AgentSpec {
        instructions: instructions(REVIEWER_CHAT_INSTRUCTIONS, settings),
        output_schema: None,
        max_turns: chat_turns(settings),
        chat: true,
        ..reviewer(settings)
    }
}

/// The resolver, which only runs when two workers' changes clash. It uses the
/// workers' model: it is the same kind of work, on a few files.
pub fn resolver(settings: &Settings) -> AgentSpec {
    let tools = names(&[READ_TOOLS, EDIT_TOOLS]);
    AgentSpec {
        id: RESOLVER_ID.to_string(),
        role: Role::Resolver,
        label: "Resolver".to_string(),
        model: settings.worker.model.clone(),
        effort: settings.worker.effort,
        instructions: instructions(RESOLVER_INSTRUCTIONS, settings),
        allowed: tools.clone(),
        denied: Vec::new(),
        tools,
        output_schema: None,
        max_budget_usd: settings.max_budget_usd,
        max_turns: RESOLVER_MAX_TURNS,
        use_mcp_servers: settings.use_mcp_servers,
        chat: false,
    }
}

/// The chat version of the agent with this ID, or `None` if you cannot message it
/// directly (the dispatcher is reached through the main message box instead).
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
    let mut agents = vec![dispatcher(settings), planner(settings)];
    agents.extend((1..=WORKER_COUNT).map(|number| worker(settings, number)));
    agents.push(reviewer(settings));
    agents.push(resolver(settings));
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Settings {
        Settings::default()
    }

    #[test]
    fn only_workers_and_the_resolver_can_edit() {
        for spec in all_agents(&defaults()) {
            let can_edit = spec.tools.iter().any(|tool| tool == "Edit" || tool == "Write");
            let writes = matches!(spec.role, Role::Worker | Role::Resolver);
            assert_eq!(can_edit, writes, "{}", spec.id);
        }
    }

    #[test]
    fn the_team_is_dispatcher_planner_three_workers_a_reviewer_and_a_resolver() {
        let ids: Vec<String> = all_agents(&defaults()).into_iter().map(|spec| spec.id).collect();
        assert_eq!(
            ids,
            ["dispatcher", "planner", "worker-1", "worker-2", "worker-3", "reviewer", "resolver"]
        );
    }

    #[test]
    fn chats_have_a_lower_turn_limit_than_tasks() {
        let settings = defaults();
        assert_eq!(worker(&settings, 1).max_turns, WORKER_MAX_TURNS);
        assert_eq!(worker_chat(&settings, 1).max_turns, CHAT_MAX_TURNS);
        assert_eq!(planner_chat(&settings).max_turns, CHAT_MAX_TURNS);
        assert!(dispatcher(&settings).max_turns < PLANNER_MAX_TURNS);
        assert!(!planner(&settings).use_mcp_servers, "MCP servers are off by default");
    }

    #[test]
    fn models_and_effort_come_from_the_settings() {
        let mut settings = defaults();
        settings.planner.effort = Some(Effort::Max);
        settings.reviewer.model = "haiku".to_string();
        assert_eq!(dispatcher(&settings).model, "haiku");
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
        assert!(!reviewer_chat.tools.iter().any(|tool| tool == "Edit"));

        assert_eq!(chat_spec(&settings, "worker-3").unwrap().id, "worker-3");
        for not_messageable in ["dispatcher", "worker-0", "worker-4", "reviewer-1", "boss", "worker"] {
            assert!(chat_spec(&settings, not_messageable).is_none(), "{not_messageable}");
        }
    }

    #[test]
    fn the_resolver_can_edit_but_not_run_anything() {
        let spec = resolver(&defaults());
        assert!(spec.tools.iter().any(|tool| tool == "Edit"));
        assert!(!spec.tools.iter().any(|tool| tool == "Bash"), "it runs inside a half-done merge");
        assert!(spec.instructions.contains("Never delete one side's work"));
    }

    #[test]
    fn allowed_commands_become_bash_rules_and_none_removes_the_shell() {
        let mut settings = defaults();
        settings.allowed_commands = vec!["npm test".into(), "npm run lint *".into()];
        let spec = worker(&settings, 1);
        assert!(spec.tools.contains(&"Bash".to_string()));
        assert!(spec.allowed.contains(&"Bash(npm test)".to_string()));
        assert!(spec.allowed.contains(&"Bash(npm run lint *)".to_string()));
        assert!(!spec.allowed.contains(&"Bash(cargo test)".to_string()));

        settings.allowed_commands.clear();
        for spec in [worker(&settings, 1), reviewer(&settings)] {
            assert!(!spec.tools.iter().any(|tool| tool == "Bash"), "{}", spec.id);
            assert!(!spec.allowed.iter().any(|rule| rule.starts_with("Bash")), "{}", spec.id);
        }
    }

    #[test]
    fn web_access_is_only_for_the_agents_that_cannot_change_anything() {
        let mut settings = defaults();
        assert!(!planner(&settings).tools.contains(&"WebSearch".to_string()), "off by default");
        settings.web_access = true;
        for spec in all_agents(&settings).into_iter().chain([planner_chat(&settings)]) {
            let has_web = spec.tools.iter().any(|tool| tool == "WebSearch" || tool == "WebFetch");
            let reads_only = matches!(spec.role, Role::Dispatcher | Role::Planner);
            assert_eq!(has_web, reads_only, "{}", spec.id);
        }
    }

    #[test]
    fn team_size_turns_and_notes_come_from_the_settings() {
        let mut settings = defaults();
        settings.worker_count = 2;
        settings.worker.max_turns = Some(15);
        settings.chat_max_turns = Some(5);
        settings.notes = "Use uv, never pip.".into();

        let plan = planner(&settings);
        assert!(plan.instructions.contains("Use at most 2,"), "{}", plan.instructions);
        assert!(!plan.instructions.contains("{max_tasks}"));
        assert_eq!(plan.output_schema.unwrap()["properties"]["tasks"]["maxItems"], 2);
        assert_eq!(worker(&settings, 1).max_turns, 15);
        assert_eq!(worker_chat(&settings, 1).max_turns, 5);
        assert_eq!(reviewer(&settings).max_turns, REVIEWER_MAX_TURNS, "unset means the default");
        for spec in all_agents(&settings) {
            assert!(spec.instructions.ends_with("Use uv, never pip."), "{}", spec.id);
        }
        assert!(!worker(&defaults(), 1).instructions.contains("Notes from the user"));
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

    #[test]
    fn any_command_allows_the_shell_outright_but_never_the_denied_ones() {
        // Nothing listed, and with "any command" on it does not matter.
        let settings = Settings { full_shell: true, allowed_commands: Vec::new(), ..Settings::default() };

        let worker = worker(&settings, 1);

        assert!(worker.tools.contains(&"Bash".to_string()));
        assert!(worker.allowed.contains(&"Bash(*)".to_string()));
        assert!(worker.denied.contains(&"Bash(git push *)".to_string()));
        assert!(worker.denied.contains(&"Bash(sudo *)".to_string()));
    }

    #[test]
    fn a_listed_command_is_allowed_and_an_unlisted_one_is_not() {
        let settings = Settings {
            full_shell: false,
            allowed_commands: vec!["uv run *".to_string()],
            ..Settings::default()
        };

        let worker = worker(&settings, 1);

        assert!(worker.allowed.contains(&"Bash(uv run *)".to_string()));
        assert!(!worker.allowed.contains(&"Bash(*)".to_string()));
        assert!(!worker.denied.is_empty(), "the deny list applies either way");
    }

    #[test]
    fn with_no_commands_at_all_the_shell_is_not_even_offered() {
        let settings = Settings { allowed_commands: Vec::new(), ..Settings::default() };

        let worker = worker(&settings, 1);

        assert!(!worker.tools.contains(&"Bash".to_string()));
        assert!(worker.denied.is_empty(), "nothing to deny when there is no shell");
    }

    #[test]
    fn the_agents_that_cannot_run_commands_need_no_deny_list() {
        let settings = Settings { full_shell: true, ..Settings::default() };
        for spec in [dispatcher(&settings), planner(&settings), resolver(&settings)] {
            assert!(!spec.tools.contains(&"Bash".to_string()), "{}", spec.id);
            assert!(spec.denied.is_empty(), "{}", spec.id);
        }
    }

    #[test]
    fn a_finding_in_nobody_s_files_is_kept_apart_from_the_workers() {
        let review = Review {
            verdict: Verdict::NeedsChanges,
            summary: "ruff fails on a file neither task owns.".to_string(),
            must_fix: vec![
                Finding { task: 1, issue: "tests/test_rules.py:4 - wrong assertion".to_string() },
                Finding { task: 0, issue: "src/glovebox/rules/__init__.py:13 - ruff PLW1508".to_string() },
            ],
            consider: vec![],
            commit_message: "fix the tests".to_string(),
        };

        assert_eq!(review.findings_for(1).len(), 1);
        assert_eq!(review.for_the_user().len(), 1);
        // Nothing meant for the user is ever handed to a worker.
        for worker in 1..=WORKER_COUNT {
            assert!(
                review.findings_for(worker).iter().all(|finding| finding.task != 0),
                "worker {worker} was given the user's finding"
            );
        }
    }

    #[test]
    fn the_reviewer_may_say_a_finding_belongs_to_no_task() {
        let schema = review_schema();
        let task = &schema["properties"]["must_fix"]["items"]["properties"]["task"];
        assert_eq!(task["minimum"], 0, "0 is how it says nobody may fix this");
    }
}
