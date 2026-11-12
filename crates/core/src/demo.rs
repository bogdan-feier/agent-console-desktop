//! Demo mode: scripted agents that cost nothing and need no login.
//!
//! Used by the "Demo mode" switch in the app and by the tests.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use crate::config::{AgentSpec, Role};
use crate::event::EventKind;
use crate::orchestrator::FIX_PROMPT_MARKER;
use crate::run::Run;
use crate::runner::{AgentError, AgentOutput, AgentRunner};

pub struct DemoRunner {
    /// Pause before each scripted step, so the UI visibly fills in.
    pub step_delay: Duration,
}

impl DemoRunner {
    pub fn new(step_delay: Duration) -> Self {
        DemoRunner { step_delay }
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
impl AgentRunner for DemoRunner {
    async fn run_session(
        &self,
        run: &Run,
        spec: &AgentSpec,
        prompt: &str,
        _resume: Option<&str>,
    ) -> Result<AgentOutput, AgentError> {
        let model = format!("demo-{}", spec.model);
        run.emit(Some(&spec.id), EventKind::AgentModel { model });
        // "worker-2" -> 2; the planner has no number.
        let number: usize = spec.id.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let session_id = Some(format!("demo-{}", spec.id));

        match spec.role {
            Role::Planner => {
                self.text(run, spec, "Looking at how the project is laid out.").await;
                self.tool(run, spec, "Glob", "src/**/*.py").await;
                self.tool(run, spec, "Read", "src/textstats/stats.py").await;
                self.tool(run, spec, "Read", "tests/test_stats.py").await;
                self.text(run, spec, "Two independent pieces, no shared files.").await;
                Ok(AgentOutput { text: String::new(), structured: Some(demo_plan()), session_id })
            }
            Role::Worker => {
                let is_fix = prompt.contains(FIX_PROMPT_MARKER);
                self.tool(run, spec, "Read", "CLAUDE.md").await;
                self.tool(run, spec, "Write", &format!("src/textstats/part{number}.py")).await;
                self.tool(run, spec, "Write", &format!("tests/test_part{number}.py")).await;
                let test_command = format!("uv run pytest tests/test_part{number}.py");
                self.tool(run, spec, "Bash", &test_command).await;
                let report = if is_fix {
                    "Fixed the empty-input case and added a test for it. 4 passed."
                } else {
                    "Added the function and three tests. 3 passed."
                };
                self.text(run, spec, report).await;
                Ok(AgentOutput { text: report.to_string(), structured: None, session_id })
            }
            Role::Reviewer => {
                // The second reviewer asks for one change the first time round.
                let first_review = run.review(number).is_none();
                self.tool(run, spec, "Bash", "git diff").await;
                self.tool(run, spec, "Read", &format!("src/textstats/part{number}.py")).await;
                self.tool(run, spec, "Bash", "uv run pytest").await;
                tokio::time::sleep(self.step_delay).await;
                let review = if number == 2 && first_review {
                    json!({
                        "verdict": "needs_changes",
                        "summary": "Works for normal text, but empty input raises instead of returning 0.",
                        "must_fix": ["src/textstats/part2.py:14 - empty string raises IndexError"],
                        "consider": ["Name the punctuation set as a constant."]
                    })
                } else {
                    json!({
                        "verdict": "approve",
                        "summary": "Does what the task asked. Tests cover normal, tie and empty input.",
                        "must_fix": [],
                        "consider": []
                    })
                };
                Ok(AgentOutput { text: String::new(), structured: Some(review), session_id })
            }
        }
    }
}

fn demo_plan() -> serde_json::Value {
    json!({
        "summary": "Add a longest-word helper and a sentence counter, each with its own tests.",
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
            }
        ]
    })
}
