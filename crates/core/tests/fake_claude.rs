//! `ClaudeRunner` against a fake `claude` program (tests/fixtures/fake-claude.sh).
//!
//! These tests start real processes, so they cover the parts unit tests cannot:
//! the command line, stdin, the working directory, the environment, and killing.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_console_core::claude::locate::ClaudeLocation;
use agent_console_core::claude::ClaudeRunner;
use agent_console_core::config::{self, AuthMode};
use agent_console_core::orchestrator::supervised;
use agent_console_core::runner::run_agent;
use agent_console_core::{Orchestrator, RunState};
use common::{new_run, Collected};
use tempfile::TempDir;

const FAKE_CLAUDE: &str = include_str!("fixtures/fake-claude.sh");

/// Write the fake program to disk once and make it executable.
fn fake_claude() -> ClaudeLocation {
    static PROGRAM: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    let (_, program) = PROGRAM.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("claude");
        fs::write(&program, FAKE_CLAUDE).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, program)
    });
    ClaudeLocation { program: program.clone(), path_variable: std::env::var("PATH").unwrap_or_default() }
}

/// A fresh project folder set up for one scenario of the fake program.
fn project(mode: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".fake-mode"), mode).unwrap();
    dir
}

fn read(project: &TempDir, name: &str) -> String {
    fs::read_to_string(project.path().join(name)).unwrap_or_default()
}

fn process_is_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn wait_for_file(path: &Path) {
    for _ in 0..500 {
        if fs::read_to_string(path).is_ok_and(|text| !text.trim().is_empty()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} never appeared", path.display());
}

#[tokio::test]
async fn a_successful_session_becomes_events_and_an_answer() {
    let project = project("ok");
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);
    let runner = ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin);

    let output = run_agent(&runner, &run, &config::planner(), "Plan this.", None).await.unwrap();

    assert_eq!(output.text, "All done.");
    assert_eq!(output.session_id.as_deref(), Some("sess-1"));
    assert_eq!(output.structured.unwrap()["tasks"][0]["title"], "Do it");

    assert_eq!(collected.statuses("planner"), ["running", "done"]);
    assert_eq!(collected.of_type("agent_model")[0]["model"], "claude-fake-1");
    assert_eq!(collected.of_type("text")[0]["text"], "Reading the project.");
    // The CLI's internal StructuredOutput tool is not shown.
    let tools: Vec<_> = collected.of_type("tool").iter().map(|e| e["name"].clone()).collect();
    assert_eq!(tools, ["Read", "Bash"]);
    assert_eq!(collected.of_type("tool")[0]["detail"], "src/a.py");
    assert!(collected.of_type("tool_error")[0]["detail"].as_str().unwrap().contains("denied"));
    assert_eq!(collected.of_type("result")[0]["cost_usd"], 0.42);
    assert_eq!(collected.of_type("result")[0]["turns"], 3);
}

#[tokio::test]
async fn the_prompt_goes_to_stdin_and_the_process_runs_in_the_project() {
    let project = project("ok");
    let run = new_run(project.path(), &Collected::default());
    let runner = ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin);

    let prompt = "Line one\nLine two with \"quotes\" and $VARIABLES";
    run_agent(&runner, &run, &config::worker(1), prompt, Some("sess-0")).await.unwrap();

    // The log files were written relative to the process's working directory.
    assert_eq!(read(&project, "fake-prompt.txt"), prompt);
    let args = read(&project, "fake-args.txt");
    for expected in ["-p", "stream-json", "--verbose", "dontAsk", "sonnet", "--resume=sess-0"] {
        assert!(args.lines().any(|arg| arg == expected), "missing {expected} in {args}");
    }
}

#[tokio::test]
async fn the_api_key_is_hidden_unless_api_key_auth_is_chosen() {
    // Set for the whole test process; both parts of this test want it set.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-test");

    let with_login = project("ok");
    let run = new_run(with_login.path(), &Collected::default());
    let runner = ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin);
    run_agent(&runner, &run, &config::planner(), "x", None).await.unwrap();
    assert_eq!(read(&with_login, "fake-key.txt").trim(), "unset");

    let with_key = project("ok");
    let run = new_run(with_key.path(), &Collected::default());
    let runner = ClaudeRunner::new(fake_claude(), AuthMode::ApiKey);
    run_agent(&runner, &run, &config::planner(), "x", None).await.unwrap();
    assert_eq!(read(&with_key, "fake-key.txt").trim(), "set");
}

#[tokio::test]
async fn a_failed_sign_in_explains_how_to_log_in() {
    let project = project("auth");
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);
    let runner = ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin);

    let error = run_agent(&runner, &run, &config::planner(), "x", None).await.unwrap_err();

    assert!(error.to_string().contains("/login"), "{error}");
    assert_eq!(collected.statuses("planner"), ["running", "error"]);
    assert_eq!(collected.of_type("error").len(), 1);
}

#[tokio::test]
async fn hitting_the_budget_cap_is_reported_by_its_name() {
    let project = project("budget");
    let run = new_run(project.path(), &Collected::default());
    let runner = ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin);

    let error = run_agent(&runner, &run, &config::worker(1), "x", None).await.unwrap_err();
    assert_eq!(error.to_string(), "error_max_budget_usd");
}

#[tokio::test]
async fn a_crash_reports_what_the_program_printed() {
    let project = project("crash");
    let run = new_run(project.path(), &Collected::default());
    let runner = ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin);

    let error = run_agent(&runner, &run, &config::planner(), "x", None).await.unwrap_err();
    assert!(error.to_string().contains("unknown option"), "{error}");
}

#[tokio::test]
async fn a_missing_program_is_a_clear_error() {
    let project = project("ok");
    let run = new_run(project.path(), &Collected::default());
    let nowhere = ClaudeLocation {
        program: PathBuf::from("/definitely/not/here/claude"),
        path_variable: String::new(),
    };
    let runner = ClaudeRunner::new(nowhere, AuthMode::ClaudeLogin);

    let error = run_agent(&runner, &run, &config::planner(), "x", None).await.unwrap_err();
    assert!(error.to_string().starts_with("Could not start claude"), "{error}");
}

#[tokio::test]
async fn a_failed_planner_fails_the_run() {
    let project = project("auth");
    let run = new_run(project.path(), &Collected::default());
    let runner = Arc::new(ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin));

    supervised(run.clone(), Orchestrator::new(runner).plan(&run)).await;
    assert_eq!(run.state(), RunState::Failed);
}

#[tokio::test]
async fn stopping_kills_the_process_and_its_children() {
    let project = project("hang");
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);
    let orchestrator = Orchestrator::new(Arc::new(ClaudeRunner::new(fake_claude(), AuthMode::ClaudeLogin)));

    let job = {
        let run = run.clone();
        tokio::spawn(async move { supervised(run.clone(), orchestrator.plan(&run)).await })
    };
    wait_for_file(&project.path().join("fake-child-pid.txt")).await;
    let pid = read(&project, "fake-pid.txt");
    let child_pid = read(&project, "fake-child-pid.txt");
    assert!(process_is_alive(&pid) && process_is_alive(&child_pid));

    assert!(run.stop());
    job.await.unwrap();

    assert_eq!(run.state(), RunState::Stopped);
    assert_eq!(collected.statuses("planner"), ["running", "stopped"]);
    // Give the operating system a moment to deliver the kill.
    for _ in 0..200 {
        if !process_is_alive(&pid) && !process_is_alive(&child_pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the fake claude process (or its child) is still running after stop");
}
