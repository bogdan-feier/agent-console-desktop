//! Running the project's own check command on the merged changes.
//!
//! This is the one piece of work in a round that no agent does. The app runs the
//! command you set in Settings (`cargo test`, `npm test`, ...) in the review
//! worktree and gives the output to the reviewer, so the reviewer does not spend
//! turns running it itself. It costs nothing and the answer is always right.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::process::Group;

/// How long the check may run before it is given up on.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
/// How much of its output is kept, in characters.
pub const MAX_OUTPUT_CHARS: usize = 8000;

/// What the check command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub command: String,
    pub passed: bool,
    /// A few words for the heading: how it ended and how long it took.
    pub note: String,
    /// What it printed, both streams together, shortened if it was long.
    pub output: String,
}

/// Run `command` in `dir` through the shell, as a terminal would.
///
/// The command is the user's own, typed into their own Settings window: it is
/// not agent input, and it is not checked for anything beyond being one line.
pub async fn run(dir: &Path, command: &str) -> Checked {
    let started = Instant::now();
    let mut building = Command::new("sh");
    building
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        // Nothing is there to type an answer, so give it no input at all.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A check command starts other programs (a compiler, a test runner). Putting
    // it in a group of its own means a check that runs too long takes them with it.
    #[cfg(unix)]
    building.process_group(0);

    let spawned = building.spawn();
    let took_so_far = || seconds(started.elapsed());
    let child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return Checked {
                command: command.to_string(),
                passed: false,
                note: format!("could not be run ({error})"),
                output: String::new(),
            }
        }
    };
    let mut group = Group(child.id());

    let result = tokio::time::timeout(CHECK_TIMEOUT, child.wait_with_output()).await;
    let took = took_so_far();
    let command = command.to_string();

    match result {
        Err(_) => Checked {
            command,
            passed: false,
            note: format!("did not finish within {} seconds, so it was stopped", CHECK_TIMEOUT.as_secs()),
            output: String::new(),
        },
        Ok(Err(error)) => Checked {
            command,
            passed: false,
            note: format!("could not be run ({error})"),
            output: String::new(),
        },
        Ok(Ok(output)) => {
            group.finished();
            let passed = output.status.success();
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            let ended = match output.status.code() {
                Some(0) => format!("passed in {took}"),
                Some(code) => format!("failed with exit code {code} after {took}"),
                None => format!("was stopped after {took}"),
            };
            Checked { command, passed, note: ended, output: shorten(text.trim_end().to_string()) }
        }
    }
}

fn seconds(elapsed: Duration) -> String {
    let whole = elapsed.as_secs();
    if whole < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}m {}s", whole / 60, whole % 60)
    }
}

/// Keep the start and the end: compilers put the problem at the top, test
/// runners at the bottom.
fn shorten(text: String) -> String {
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= MAX_OUTPUT_CHARS {
        return text;
    }
    let half = MAX_OUTPUT_CHARS / 2;
    let head: String = characters[..half].iter().collect();
    let tail: String = characters[characters.len() - half..].iter().collect();
    let skipped = characters.len() - MAX_OUTPUT_CHARS;
    format!("{head}\n\n... ({skipped} characters left out) ...\n\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_command_that_works_passes_and_keeps_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let checked = run(dir.path(), "echo hello; echo trouble >&2").await;
        assert!(checked.passed);
        assert!(checked.note.starts_with("passed in"), "{}", checked.note);
        assert!(checked.output.contains("hello") && checked.output.contains("trouble"));
    }

    #[tokio::test]
    async fn a_failing_command_says_what_it_returned() {
        let dir = tempfile::tempdir().unwrap();
        let checked = run(dir.path(), "echo nope; exit 3").await;
        assert!(!checked.passed);
        assert!(checked.note.contains("exit code 3"), "{}", checked.note);
    }

    #[tokio::test]
    async fn a_check_that_runs_too_long_takes_what_it_started_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-running.txt");
        // A shell that starts a long-lived child and then waits forever itself.
        let command = format!("(while true; do echo x >> {}; sleep 0.05; done) & sleep 60", marker.display());
        let soon = tokio::time::timeout(Duration::from_millis(700), run(dir.path(), &command)).await;
        // The check itself is still going when we stop waiting; dropping it kills
        // the group, so the child stops writing.
        assert!(soon.is_err(), "the check was still running");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let size = std::fs::metadata(&marker).map(|data| data.len()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let later = std::fs::metadata(&marker).map(|data| data.len()).unwrap_or(0);
        assert_eq!(size, later, "nothing it started is still writing");
    }

    #[tokio::test]
    async fn it_runs_in_the_folder_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("here.txt"), "x").await.unwrap();
        assert!(run(dir.path(), "test -f here.txt").await.passed);
    }

    #[test]
    fn long_output_keeps_both_ends() {
        let text = format!("{}{}{}", "start", "x".repeat(MAX_OUTPUT_CHARS * 2), "end");
        let short = shorten(text);
        assert!(short.starts_with("start") && short.ends_with("end"));
        assert!(short.contains("characters left out"));
        assert!(short.chars().count() < MAX_OUTPUT_CHARS + 200);
    }
}
