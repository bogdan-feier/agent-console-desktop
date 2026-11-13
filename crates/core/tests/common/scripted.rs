//! Scripted agents for the tests: they cost nothing, need no login, and never
//! touch any files. They only report what a real agent might do.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use agent_console_core::config::{AgentSpec, Role};
use agent_console_core::event::{EventKind, TokenUsage};
use agent_console_core::orchestrator::FIX_PROMPT_MARKER;
use agent_console_core::{AgentError, AgentOutput, AgentRunner, Run};

/// Words that make the scripted dispatcher treat a message as a request to build something.
const BUILD_WORDS: &[&str] = &["add", "build", "implement", "create", "make", "write", "fix", "plan"];
/// Words that make it treat a message as a quick question it can answer itself.
const QUESTION_WORDS: &[&str] = &["what", "where", "which", "how do", "how does", "how are", "who"];

pub struct ScriptedRunner {
    /// Pause before each scripted step, so the UI visibly fills in.
    pub step_delay: Duration,
}

impl ScriptedRunner {
    pub fn new(step_delay: Duration) -> Self {
        ScriptedRunner { step_delay }
    }

    async fn text(&self, run: &Run, spec: &AgentSpec, text: &str) {
        tokio::time::sleep(self.step_delay).await;
        run.emit(Some(&spec.id), EventKind::Text { text: text.to_string() });
    }

    async fn tool(&self, run: &Run, spec: &AgentSpec, name: &str, detail: &str) {
        tokio::time::sleep(self.step_delay).await;
        let kind = EventKind::Tool { name: name.to_string(), detail: detail.to_string() };
        run.emit(Some(&spec.id), kind);
    }
}

#[async_trait]
impl AgentRunner for ScriptedRunner {
    async fn run_session(
        &self,
        run: &Run,
        spec: &AgentSpec,
        prompt: &str,
        _resume: Option<&str>,
        _folder: &Path,
    ) -> Result<AgentOutput, AgentError> {
        run.emit(Some(&spec.id), EventKind::AgentModel { model: format!("scripted-{}", spec.model) });
        let (text, structured) = match (spec.role, spec.chat) {
            (Role::Dispatcher, _) => (String::new(), Some(self.dispatcher(run, spec, prompt).await)),
            (Role::Planner, false) => (String::new(), Some(self.plan(run, spec).await)),
            (Role::Worker, false) => (self.work(run, spec, prompt).await, None),
            (Role::Reviewer, false) => (String::new(), Some(self.review(run, spec).await)),
            (_, true) => (self.chat_reply(run, spec, prompt).await, None),
        };
        // Made-up but realistic numbers, so usage reporting is exercised too.
        let tokens = TokenUsage {
            input_tokens: 1_800,
            output_tokens: 900,
            cache_read_input_tokens: 38_000,
            cache_creation_input_tokens: 6_500,
        };
        run.emit(Some(&spec.id), EventKind::Result { cost_usd: None, turns: Some(4), tokens });
        Ok(AgentOutput { text, structured, session_id: Some(format!("scripted-{}", spec.id)) })
    }
}

impl ScriptedRunner {
    async fn dispatcher(&self, run: &Run, spec: &AgentSpec, message: &str) -> Value {
        let lower = message.to_lowercase();
        let has_word = |words: &[&str]| words.iter().any(|word| lower.contains(word));
        if has_word(QUESTION_WORDS) && !has_word(BUILD_WORDS) {
            self.tool(run, spec, "Grep", "def ").await;
            json!({
                "route": "answer",
                "reply": "`stats.py` has three functions: `count_words`, `average_word_length` and \
                          `most_common_words`. They all split text with the same `_words` helper. \
                          Tests are in `tests/test_stats.py`; run them with `uv run pytest`."
            })
        } else if has_word(BUILD_WORDS) {
            json!({ "route": "plan", "reply": "That is a clear change, so I passed it to the planner to plan." })
        } else {
            json!({ "route": "brainstorm", "reply": "I passed this to the planner to talk it through." })
        }
    }

    async fn plan(&self, run: &Run, spec: &AgentSpec) -> Value {
        self.text(run, spec, "Looking at how the project is laid out.").await;
        self.tool(run, spec, "Glob", "src/**/*.py").await;
        self.tool(run, spec, "Read", "src/textstats/stats.py").await;
        self.text(run, spec, "Three independent helpers, each in its own module: no shared files.").await;
        json!({
            "summary": "Add three text helpers, each in its own module with its own tests.",
            "tasks": [
                {
                    "title": "Add longest_word()",
                    "instructions": "Add longest_word(text) to src/textstats/words.py. Return the longest word, or an empty string for empty text. Add tests for normal, tie and empty input.",
                    "files": ["src/textstats/words.py", "tests/test_words.py"]
                },
                {
                    "title": "Add count_sentences()",
                    "instructions": "Add count_sentences(text) to src/textstats/sentences.py. Count sentences ending in '.', '!' or '?'. Add tests for normal, empty and no-punctuation input.",
                    "files": ["src/textstats/sentences.py", "tests/test_sentences.py"]
                },
                {
                    "title": "Add reading_time()",
                    "instructions": "Add reading_time(text, words_per_minute=200) to src/textstats/reading.py, returning whole minutes rounded up. Add tests for empty, short and long text.",
                    "files": ["src/textstats/reading.py", "tests/test_reading.py"]
                }
            ]
        })
    }

    async fn work(&self, run: &Run, spec: &AgentSpec, prompt: &str) -> String {
        let number = spec.id.trim_start_matches("worker-");
        self.tool(run, spec, "Read", "CLAUDE.md").await;
        self.tool(run, spec, "Write", &format!("src/textstats/part{number}.py")).await;
        self.tool(run, spec, "Write", &format!("tests/test_part{number}.py")).await;
        self.tool(run, spec, "Bash", &format!("uv run pytest tests/test_part{number}.py")).await;
        let report = if prompt.contains(FIX_PROMPT_MARKER) {
            "Fixed the empty-input case and added a test for it. 4 passed."
        } else {
            "Added the function and three tests. 3 passed."
        };
        self.text(run, spec, report).await;
        report.to_string()
    }

    /// The first review asks worker 2 for one fix; later reviews approve.
    async fn review(&self, run: &Run, spec: &AgentSpec) -> Value {
        let first_review = run.review().is_none();
        self.tool(run, spec, "Read", "src/textstats/sentences.py").await;
        self.tool(run, spec, "Bash", "uv run pytest").await;
        tokio::time::sleep(self.step_delay).await;
        if first_review {
            json!({
                "verdict": "needs_changes",
                "summary": "Tasks 1 and 3 are fine. Task 2 raises on empty input instead of returning 0.",
                "must_fix": [{ "task": 2, "issue": "src/textstats/sentences.py:14 - empty string raises IndexError" }],
                "consider": ["Task 2: name the punctuation set as a constant."]
            })
        } else {
            json!({
                "verdict": "approve",
                "summary": "All three tasks do what was asked, the tests pass, and the modules fit together.",
                "must_fix": [],
                "consider": []
            })
        }
    }

    /// A scripted answer to a message from the user.
    async fn chat_reply(&self, run: &Run, spec: &AgentSpec, prompt: &str) -> String {
        // The first line of the user's message, shortened, so the reply visibly answers it.
        let topic: String = prompt.lines().next().unwrap_or_default().chars().take(80).collect();
        let reply = match spec.role {
            Role::Planner | Role::Dispatcher => {
                self.tool(run, spec, "Grep", "def ").await;
                self.tool(run, spec, "Read", "src/textstats/stats.py").await;
                format!(
                    "On \"{topic}\": the project already has a word splitter in `stats.py`, so new \
                     helpers can reuse it. I see two ways to split the work:\n\
                     1. One helper per worker, each in its own module with its own tests.\n\
                     2. One worker writes everything, which is simpler but slower.\n\
                     Option 1 keeps the workers out of each other's files. Which do you prefer?"
                )
            }
            Role::Worker => {
                self.tool(run, spec, "Edit", "src/textstats/words.py").await;
                self.tool(run, spec, "Bash", "uv run pytest tests/test_words.py").await;
                format!("Done: \"{topic}\". I changed `words.py` and the tests still pass (3 passed).")
            }
            Role::Reviewer => {
                self.tool(run, spec, "Read", "src/textstats/sentences.py").await;
                format!(
                    "About \"{topic}\": the empty-input case is the only real problem. The constant is \
                     a style point; I would not block on it."
                )
            }
        };
        self.text(run, spec, &reply).await;
        reply
    }
}
