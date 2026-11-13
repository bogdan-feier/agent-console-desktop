//! The only code that talks to the `claude` program.
//!
//! [`ClaudeRunner`] starts `claude` as a child process, writes the prompt to
//! its standard input, and reads its standard output line by line. Each line
//! is a JSON message (see `protocol.rs`) that becomes an event for the UI.

pub mod command;
pub mod locate;
pub mod protocol;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::{self, AgentSpec, AuthMode};
use crate::event::EventKind;
use crate::run::Run;
use crate::runner::{AgentError, AgentOutput, AgentRunner};

use locate::ClaudeLocation;
use protocol::{parse_line, tool_result_text, Block, Line, ResultLine};

const API_KEY_VARIABLE: &str = "ANTHROPIC_API_KEY";
/// Longest tool detail or error snippet shown in the UI, in characters.
const DETAIL_LIMIT: usize = 200;
/// The CLI's own tool for returning the structured answer; not worth showing.
const STRUCTURED_OUTPUT_TOOL: &str = "StructuredOutput";

pub struct ClaudeRunner {
    location: ClaudeLocation,
    auth: AuthMode,
}

impl ClaudeRunner {
    pub fn new(location: ClaudeLocation, auth: AuthMode) -> Self {
        ClaudeRunner { location, auth }
    }

    fn auth_hint(&self) -> &'static str {
        match self.auth {
            AuthMode::ClaudeLogin => {
                "Claude could not sign in with your Claude Code login. In a terminal, run \
                 `claude`, type /login and finish signing in, then try again here."
            }
            AuthMode::ApiKey => {
                "Claude could not sign in. Start Agent Console from a terminal where \
                 ANTHROPIC_API_KEY is set, then try again."
            }
        }
    }
}

#[async_trait]
impl AgentRunner for ClaudeRunner {
    fn parallel_start_gap(&self) -> Duration {
        config::PARALLEL_START_GAP
    }

    async fn run_session(
        &self,
        run: &Run,
        spec: &AgentSpec,
        prompt: &str,
        resume: Option<&str>,
        folder: &Path,
    ) -> Result<AgentOutput, AgentError> {
        let agent = spec.id.as_str();

        let mut command = Command::new(&self.location.program);
        command
            .args(command::build_args(spec, resume, self.location.known_flags.as_ref()))
            .current_dir(folder)
            .env("PATH", &self.location.path_variable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If this future is dropped (the user pressed Stop), kill the process too.
            .kill_on_drop(true);
        // Put claude in a process group of its own, so that everything it
        // starts (a long test run, say) can be stopped together with it.
        #[cfg(unix)]
        command.process_group(0);
        if self.auth == AuthMode::ClaudeLogin {
            // An API key would win over the login and bill the API account instead.
            command.env_remove(API_KEY_VARIABLE);
        }

        let mut child =
            command.spawn().map_err(|error| AgentError(format!("Could not start claude: {error}")))?;

        let mut group = ProcessGroupGuard(child.id());

        // Send the prompt, then close stdin so claude knows the prompt is complete.
        let mut stdin = child.stdin.take().expect("stdin was requested above");
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|error| AgentError(format!("Could not send the prompt to claude: {error}")))?;
        drop(stdin);

        // Collect stderr in the background so a chatty process can never block on it.
        let mut stderr = child.stderr.take().expect("stderr was requested above");
        let stderr_text = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });

        let stdout = child.stdout.take().expect("stdout was requested above");
        let mut lines = BufReader::new(stdout).lines();
        let mut final_line: Option<ResultLine> = None;
        let mut auth_failed = false;

        while let Ok(Some(line)) = lines.next_line().await {
            match parse_line(&line) {
                Some(Line::System { subtype, model: Some(model) }) if subtype == "init" => {
                    run.emit(Some(agent), EventKind::AgentModel { model });
                }
                Some(Line::Assistant { message, error }) => {
                    auth_failed |= error.as_deref() == Some("authentication_failed");
                    for block in message.blocks() {
                        match block {
                            Block::Text { text } if !text.trim().is_empty() => {
                                run.emit(Some(agent), EventKind::Text { text });
                            }
                            Block::ToolUse { name, input } if name != STRUCTURED_OUTPUT_TOOL => {
                                let detail = describe_tool_call(&name, &input);
                                run.emit(Some(agent), EventKind::Tool { name, detail });
                            }
                            _ => {}
                        }
                    }
                }
                Some(Line::User { message }) => {
                    for block in message.blocks() {
                        if let Block::ToolResult { content, is_error: Some(true) } = block {
                            let detail = shorten(&tool_result_text(&content));
                            run.emit(Some(agent), EventKind::ToolError { detail });
                        }
                    }
                }
                Some(Line::Result(result)) => final_line = Some(result),
                _ => {}
            }
        }
        let _ = child.wait().await;
        // The process ended on its own: nothing left to kill.
        group.0 = None;

        let Some(result) = final_line else {
            let stderr_text = stderr_text.await.unwrap_or_default();
            let reason = stderr_text.trim();
            return Err(AgentError(if reason.is_empty() {
                "claude stopped without giving a final answer.".to_string()
            } else {
                format!("claude stopped early: {}", shorten(reason))
            }));
        };
        // Report what the session used, whether it succeeded or stopped at a limit.
        let usage = EventKind::Result {
            cost_usd: result.total_cost_usd,
            turns: result.num_turns,
            tokens: result.usage,
        };
        run.emit(Some(agent), usage);

        if result.is_error {
            if auth_failed {
                return Err(AgentError(self.auth_hint().to_string()));
            }
            // The session itself is fine (it stopped at a limit, say), so keep it: a
            // message to this agent continues where it stopped instead of starting over.
            if let Some(session) = &result.session_id {
                run.record_session(agent, session.clone());
            }
            return Err(AgentError(describe_failure(&result)));
        }

        Ok(AgentOutput {
            text: result.result.unwrap_or_default(),
            structured: result.structured_output,
            session_id: result.session_id,
        })
    }
}

/// A readable explanation of why a session ended with an error.
fn describe_failure(result: &ResultLine) -> String {
    match result.subtype.as_str() {
        "error_max_turns" => "Stopped at this agent's turn limit (Settings, Team and limits). \
                              Send it a message to let it continue, or raise the limit."
            .to_string(),
        "error_max_budget_usd" => "Stopped at the spending cap from Settings. Send it a message to \
                                   let it continue, or raise the cap."
            .to_string(),
        _ => result.result.clone().unwrap_or_else(|| result.subtype.clone()),
    }
}

/// Kills a whole process group when dropped.
///
/// `kill_on_drop` only kills `claude` itself. Commands that claude started
/// would keep running, so when a session is dropped early (the user pressed
/// Stop) the whole group is told to quit.
struct ProcessGroupGuard(Option<u32>);

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.0 {
            // `kill -KILL -<group>` signals every process in the group. Using
            // the system's `kill` program avoids unsafe code for one system call.
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{group}")])
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// The one field of a tool call worth showing: the file, the command or the pattern.
fn describe_tool_call(name: &str, input: &Value) -> String {
    let field = match name {
        "Read" | "Edit" | "Write" => "file_path",
        "Bash" => "command",
        "Grep" | "Glob" => "pattern",
        _ => return String::new(),
    };
    shorten(input.get(field).and_then(Value::as_str).unwrap_or_default())
}

fn shorten(text: &str) -> String {
    text.chars().take(DETAIL_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_calls_show_the_useful_field() {
        assert_eq!(describe_tool_call("Read", &json!({"file_path": "src/a.py"})), "src/a.py");
        assert_eq!(describe_tool_call("Bash", &json!({"command": "cargo test"})), "cargo test");
        assert_eq!(describe_tool_call("Grep", &json!({"pattern": "fn main"})), "fn main");
        assert_eq!(describe_tool_call("Mystery", &json!({"x": 1})), "");
    }

    #[test]
    fn long_details_are_cut_on_a_character_boundary() {
        let long = "é".repeat(DETAIL_LIMIT + 50);
        assert_eq!(shorten(&long).chars().count(), DETAIL_LIMIT);
    }
}
