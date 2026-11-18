//! The only code that talks to the `claude` program.
//!
//! [`ClaudeRunner`] starts `claude` as a child process, writes the prompt to
//! its standard input, and reads its standard output line by line. Each line
//! is a JSON message (see `protocol.rs`) that becomes an event for the UI.

pub mod command;
pub mod locate;
pub mod protocol;
pub mod sandbox;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::{self, AgentSpec, AuthMode};
use crate::event::EventKind;
use crate::process::Group;
use crate::run::Run;
use crate::runner::{AgentError, AgentOutput, AgentRunner};
use crate::worktrees;

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

        // "Any command" promises that pushing, sudo and the rest are still refused,
        // and that promise rests on a flag an older claude may not have. Say so
        // rather than let the window claim a limit that is not being applied.
        if command::deny_list_ignored(spec, self.location.known_flags.as_ref()) {
            run.emit(
                Some(agent),
                EventKind::Notice {
                    text: "This version of claude cannot be told which commands to refuse, so \
                           \"any command\" really does mean any command: pushing, sudo and \
                           reaching the network are not blocked. Updating Claude Code restores \
                           those limits."
                        .to_string(),
                },
            );
        }

        let mut args = command::build_args(spec, resume, self.location.known_flags.as_ref());
        if spec.sandboxed {
            // "Any command" without the sandbox would be any command anywhere on this
            // machine, so an agent that cannot be sandboxed does not start at all.
            let known = self.location.known_flags.as_ref();
            if known.is_some_and(|flags| !flags.contains(sandbox::SETTINGS_FLAG)) {
                let message = "This version of Claude Code cannot run commands in a sandbox, so \"any \
                               command\" cannot be kept inside the project. Update Claude Code, or turn \
                               off \"any command\" in Settings."
                    .to_string();
                run.emit(Some(agent), EventKind::Error { message: message.clone(), tidy_with: None });
                return Err(AgentError(message));
            }
            let git_dir = worktrees::git_dir(folder).await;
            let others = sandbox::other_round_folders(folder, &worktrees::app_root());
            args.push(sandbox::SETTINGS_FLAG.to_string());
            args.push(sandbox::settings(git_dir.as_deref(), &others).to_string());
        }

        let mut command = Command::new(&self.location.program);
        command
            .args(args)
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
        command.envs(command::environment(spec.role));
        if self.auth == AuthMode::ClaudeLogin {
            // An API key would win over the login and bill the API account instead.
            command.env_remove(API_KEY_VARIABLE);
        }

        let mut child =
            command.spawn().map_err(|error| AgentError(format!("Could not start claude: {error}")))?;

        let mut group = Group(child.id());

        // Collect stderr in the background so a chatty process can never block on
        // it, and so it is already being read if the next step goes wrong.
        let mut stderr = child.stderr.take().expect("stderr was requested above");
        let stderr_text = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });

        // Send the prompt, then close stdin so claude knows the prompt is complete.
        let mut stdin = child.stdin.take().expect("stdin was requested above");
        if let Err(error) = stdin.write_all(prompt.as_bytes()).await {
            // A broken pipe means claude was already gone, and what it printed on
            // its way out says why. That is the message worth showing, not ours.
            drop(stdin);
            let _ = child.wait().await;
            group.finished();
            let said = stderr_text.await.unwrap_or_default();
            return Err(AgentError(match said.trim() {
                "" => format!("Could not send the prompt to claude: {error}"),
                reason => format!("claude stopped before it could be given the prompt: {}", shorten(reason)),
            }));
        }
        drop(stdin);

        let stdout = child.stdout.take().expect("stdout was requested above");
        let mut lines = BufReader::new(stdout).lines();
        let mut final_line: Option<ResultLine> = None;
        let mut auth_failed = false;
        // The last shell command the agent tried, so that if the next thing we see
        // is a refusal we can offer to allow it.
        let mut last_command: Option<String> = None;

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
                                // Only the latest call counts: a refused Read must not be
                                // labelled with a shell command from earlier on.
                                last_command = if name == "Bash" {
                                    input.get("command").and_then(Value::as_str).map(str::to_string)
                                } else {
                                    None
                                };
                                run.emit(Some(agent), EventKind::Tool { name, detail });
                            }
                            _ => {}
                        }
                    }
                }
                Some(Line::User { message }) => {
                    for block in message.blocks() {
                        if let Block::ToolResult { content, is_error: Some(true) } = block {
                            let text = tool_result_text(&content);
                            if is_malformed_call(&text) {
                                continue;
                            }
                            let allow_rule = refused_command(&text, last_command.as_deref());
                            let detail = tool_error_detail(&text, last_command.as_deref());
                            run.emit(Some(agent), EventKind::ToolError { detail, allow_rule });
                        }
                    }
                }
                Some(Line::Result(result)) => final_line = Some(result),
                _ => {}
            }
        }
        let _ = child.wait().await;
        // The process ended on its own: nothing left to kill.
        group.finished();

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

/// The one field of a tool call worth showing: the file, the command or the pattern.
fn describe_tool_call(name: &str, input: &Value) -> String {
    let field = match name {
        "Read" | "Edit" | "Write" => "file_path",
        "Bash" => "command",
        "Grep" | "Glob" => "pattern",
        "Skill" => "skill",
        _ => return String::new(),
    };
    shorten(input.get(field).and_then(Value::as_str).unwrap_or_default())
}

/// Whether a tool call failed only because the model got its arguments wrong. It
/// retries on its own straight away, so a red line for it is noise.
fn is_malformed_call(result_text: &str) -> bool {
    result_text.contains("<tool_use_error>InputValidationError")
}

/// Whether a tool result says the call was refused for want of permission, rather
/// than run and failed.
fn is_refusal(result_text: &str) -> bool {
    let said = result_text.to_lowercase();
    said.contains("permission")
        || said.contains("not allowed")
        || said.contains("haven't granted")
        || said.contains("has not been granted")
}

/// When a shell command was refused for want of permission, the rule that would
/// allow commands like it. `None` when the command simply failed: that is the
/// project's problem, not a permission to grant.
fn refused_command(result_text: &str, command: Option<&str>) -> Option<String> {
    if !is_refusal(result_text) {
        return None;
    }
    allow_rule(command?)
}

/// What a failed tool call's card says. For a refused shell command, the command
/// itself: Claude Code's refusal is a long paragraph that never names it, and the
/// command is what you need to decide whether to allow it.
fn tool_error_detail(result_text: &str, command: Option<&str>) -> String {
    match command {
        Some(command) if is_refusal(result_text) => format!("not allowed: {}", shorten(command)),
        _ => shorten(result_text),
    }
}

/// "npm run lint --fix" -> "npm run *", "pytest -q" -> "pytest *".
///
/// The program, plus its first word if that word is a subcommand rather than an
/// option, and then anything. Narrow enough to be worth seeing before you agree
/// to it, wide enough to be worth agreeing to.
fn allow_rule(command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    let program = words.next()?;
    let mut rule = program.to_string();
    if let Some(second) = words.next() {
        if !second.starts_with('-') {
            rule.push(' ');
            rule.push_str(second);
        }
    }
    // The rule becomes `Bash(<rule>)` in a comma-separated list, so these would
    // break it. A command with them in its name is left for the Settings window.
    if rule.contains([',', '(', ')']) {
        return None;
    }
    Some(rule + " *")
}

fn shorten(text: &str) -> String {
    text.chars().take(DETAIL_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_refused_command_can_be_offered_as_a_rule() {
        let refused = "Claude requested permissions to use Bash, but you have not granted it yet.";
        assert_eq!(refused_command(refused, Some("npm run lint --fix")), Some("npm run *".to_string()));
        assert_eq!(refused_command(refused, Some("pytest -q")), Some("pytest *".to_string()));
        assert_eq!(refused_command(refused, Some("make")), Some("make *".to_string()));
        // A command that simply failed is not a permission to grant.
        assert_eq!(refused_command("error: test failed", Some("cargo test")), None);
        // Nothing to offer when it was not the shell that was refused.
        assert_eq!(refused_command(refused, None), None);
        // A rule we could not store safely is not offered at all.
        assert_eq!(refused_command(refused, Some("foo(bar)")), None);
    }

    #[test]
    fn only_malformed_calls_are_left_off_the_cards() {
        let malformed = "<tool_use_error>InputValidationError: Read failed due to the following issues:\n\
                         The required parameter `file_path` is missing</tool_use_error>";
        assert!(is_malformed_call(malformed));
        // Still shown: these say something about the setup, not a typo the model fixes.
        assert!(!is_malformed_call("<tool_use_error>Unknown skill: dev-conventions:nope</tool_use_error>"));
        assert!(!is_malformed_call("Permission to use Bash has been denied."));
    }

    #[test]
    fn a_refused_command_is_named_on_its_card() {
        let refused =
            "Permission to use Bash has been denied because Claude Code is running in don't ask mode.";
        assert_eq!(
            tool_error_detail(refused, Some("uv run pytest -q && uv run ruff check src")),
            "not allowed: uv run pytest -q && uv run ruff check src"
        );
        // A command that ran and failed keeps its own message.
        assert_eq!(tool_error_detail("error: 2 tests failed", Some("cargo test")), "error: 2 tests failed");
        // Not a shell call: the refusal text is all there is.
        assert_eq!(tool_error_detail(refused, None), refused);
    }

    #[test]
    fn tool_calls_show_the_useful_field() {
        assert_eq!(describe_tool_call("Read", &json!({"file_path": "src/a.py"})), "src/a.py");
        assert_eq!(describe_tool_call("Bash", &json!({"command": "cargo test"})), "cargo test");
        assert_eq!(describe_tool_call("Grep", &json!({"pattern": "fn main"})), "fn main");
        assert_eq!(
            describe_tool_call("Skill", &json!({"skill": "dev-conventions:minimal-change"})),
            "dev-conventions:minimal-change"
        );
        assert_eq!(describe_tool_call("Mystery", &json!({"x": 1})), "");
    }

    #[test]
    fn long_details_are_cut_on_a_character_boundary() {
        let long = "é".repeat(DETAIL_LIMIT + 50);
        assert_eq!(shorten(&long).chars().count(), DETAIL_LIMIT);
    }
}
