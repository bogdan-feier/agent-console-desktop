//! A whole round in real git worktrees, with stand-in agents that really write files.

mod common;

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_console_core::config::{AgentSpec, Role};
use agent_console_core::orchestrator::{supervised, Isolation, FIX_PROMPT_MARKER};
use agent_console_core::settings::{self, Settings};
use agent_console_core::{AgentError, AgentOutput, AgentRunner, Orchestrator, Run, RunState};
use common::{new_run, Collected};
use serde_json::json;

/// Plans two tasks; worker N writes `fileN.py` in whatever folder it is given; the
/// reviewer asks worker 1 for a fix the first time and approves the second time.
#[derive(Default)]
struct WritingRunner {
    /// (agent, folder it ran in, prompt), for every session.
    sessions: Mutex<Vec<(String, String, String)>>,
}

#[async_trait::async_trait]
impl AgentRunner for WritingRunner {
    async fn run_session(
        &self,
        run: &Run,
        spec: &AgentSpec,
        prompt: &str,
        _resume: Option<&str>,
        folder: &Path,
    ) -> Result<AgentOutput, AgentError> {
        let reviews_so_far =
            self.sessions.lock().unwrap().iter().filter(|(id, _, _)| id == "reviewer").count();
        self.sessions.lock().unwrap().push((
            spec.id.clone(),
            folder.display().to_string(),
            prompt.to_string(),
        ));
        let structured = match spec.role {
            Role::Planner => Some(json!({
                "summary": "Two files.",
                "tasks": [
                    { "title": "File one", "instructions": "Write file1.py", "files": ["file1.py"] },
                    { "title": "File two", "instructions": "Write file2.py", "files": ["file2.py"] }
                ]
            })),
            Role::Worker => {
                let number = spec.id.trim_start_matches("worker-");
                let content =
                    if prompt.contains(FIX_PROMPT_MARKER) { "value = 'fixed'\n" } else { "value = 1\n" };
                fs::write(folder.join(format!("file{number}.py")), content).unwrap();
                None
            }
            Role::Reviewer if reviews_so_far == 0 => Some(json!({
                "verdict": "needs_changes", "summary": "One problem.", "consider": [],
                "must_fix": [{ "task": 1, "issue": "file1.py:1 - wrong value" }]
            })),
            Role::Reviewer => {
                Some(json!({ "verdict": "approve", "summary": "Good.", "must_fix": [], "consider": [] }))
            }
            Role::Jarvis => None,
        };
        let _ = run;
        Ok(AgentOutput {
            text: format!("{} done", spec.id),
            structured,
            session_id: Some(format!("s-{}", spec.id)),
        })
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git is installed");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    fs::write(dir.path().join("README.md"), "project\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "start"]);
    dir
}

fn orchestrator(runner: Arc<WritingRunner>) -> Orchestrator {
    Orchestrator::new(runner, settings::shared(Settings::default()), Isolation::Worktrees)
}

#[tokio::test]
async fn workers_build_in_their_own_worktrees_and_apply_stages_the_result() {
    let project = project();
    let runner = Arc::new(WritingRunner::default());
    let orchestrator = orchestrator(runner.clone());
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;

    assert_eq!(run.state(), RunState::Done);
    // The workers worked elsewhere: the project folder has not changed yet.
    assert!(!project.path().join("file1.py").exists());
    assert!(git(project.path(), &["status", "--porcelain"]).is_empty());
    let folders: Vec<(String, String)> =
        runner.sessions.lock().unwrap().iter().map(|(id, folder, _)| (id.clone(), folder.clone())).collect();
    let folder_of = |agent: &str| folders.iter().find(|(id, _)| id == agent).unwrap().1.clone();
    assert!(folder_of("worker-1").ends_with("worker-1"));
    assert!(folder_of("reviewer").ends_with("review"));
    assert_eq!(folder_of("planner"), project.path().display().to_string());

    // The reviewer saw both workers' changes together.
    let first_review = runner.sessions.lock().unwrap()[3].2.clone();
    assert!(first_review.contains("file1.py") && first_review.contains("file2.py"), "{first_review}");

    // Worker 1 fixes its file; the second review only sees that change.
    supervised(run.clone(), orchestrator.send_back(&run, 1)).await;
    let sessions = runner.sessions.lock().unwrap().clone();
    let (fix_agent, fix_folder, _) = &sessions[4];
    assert_eq!((fix_agent.as_str(), fix_folder.as_str()), ("worker-1", folder_of("worker-1").as_str()));
    let second_review = &sessions[5].2;
    assert!(second_review.contains("+value = 'fixed'"), "{second_review}");
    assert!(!second_review.contains("file2.py"), "only the change since the first review");

    supervised(run.clone(), orchestrator.apply(&run)).await;
    assert_eq!(run.state(), RunState::Ready);
    assert_eq!(fs::read_to_string(project.path().join("file1.py")).unwrap(), "value = 'fixed'\n");
    let status = git(project.path(), &["status", "--porcelain"]);
    assert!(status.contains("A  file1.py") && status.contains("A  file2.py"), "staged: {status}");
    assert_eq!(git(project.path(), &["log", "--oneline"]).lines().count(), 1, "nothing committed for you");
    assert_eq!(git(project.path(), &["worktree", "list"]).lines().count(), 1, "worktrees removed");
    assert!(git(project.path(), &["branch", "--list", "agent-console/*"]).trim().is_empty());
}

#[tokio::test]
async fn approving_with_uncommitted_changes_starts_nothing_and_keeps_the_plan() {
    let project = project();
    fs::write(project.path().join("README.md"), "edited\n").unwrap();
    let runner = Arc::new(WritingRunner::default());
    let orchestrator = orchestrator(runner.clone());
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;

    assert_eq!(run.state(), RunState::AwaitingApproval);
    assert!(!run.has_open_round());
    let error = collected.of_type("error").last().unwrap()["message"].as_str().unwrap().to_string();
    assert!(error.contains("uncommitted changes"), "{error}");
    assert_eq!(runner.sessions.lock().unwrap().len(), 1, "only the planner ran");
}

#[tokio::test]
async fn discarding_removes_the_worktrees_and_leaves_the_project_alone() {
    let project = project();
    let runner = Arc::new(WritingRunner::default());
    let orchestrator = orchestrator(runner.clone());
    let run = new_run(project.path(), &Collected::default());

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;
    supervised(run.clone(), orchestrator.discard(&run)).await;

    assert_eq!(run.state(), RunState::Ready);
    assert!(git(project.path(), &["status", "--porcelain"]).is_empty());
    assert_eq!(git(project.path(), &["worktree", "list"]).lines().count(), 1);
    assert!(git(project.path(), &["branch", "--list", "agent-console/*"]).trim().is_empty());
}
