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
            Role::Dispatcher | Role::Resolver => None,
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
    // A real project is committed to by whoever owns it, so "apply and commit" uses
    // the project's own identity, not ours. Give this one an identity to use; a
    // machine with no git identity at all (a CI runner) has none to fall back on.
    git(dir.path(), &["config", "user.name", "Test"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
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

    supervised(run.clone(), orchestrator.apply(&run, None, false)).await;
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

/// Both workers edit the same lines of the same file, which git cannot merge.
/// The resolver writes a version with both changes in it.
struct ClashingRunner {
    /// (agent, folder it ran in), for every session.
    sessions: Mutex<Vec<(String, String)>>,
    /// What the resolver writes; `None` means it leaves the markers alone.
    resolution: Option<&'static str>,
}

impl ClashingRunner {
    fn new(resolution: Option<&'static str>) -> Arc<ClashingRunner> {
        Arc::new(ClashingRunner { sessions: Mutex::new(Vec::new()), resolution })
    }

    fn ran(&self, agent: &str) -> bool {
        self.sessions.lock().unwrap().iter().any(|(id, _)| id == agent)
    }
}

#[async_trait::async_trait]
impl AgentRunner for ClashingRunner {
    async fn run_session(
        &self,
        _run: &Run,
        spec: &AgentSpec,
        _prompt: &str,
        _resume: Option<&str>,
        folder: &Path,
    ) -> Result<AgentOutput, AgentError> {
        self.sessions.lock().unwrap().push((spec.id.clone(), folder.display().to_string()));
        let structured = match spec.role {
            Role::Planner => Some(json!({
                "summary": "Both tasks touch shared.py.",
                "tasks": [
                    { "title": "One", "instructions": "Edit shared.py", "files": ["shared.py"] },
                    { "title": "Two", "instructions": "Edit shared.py too", "files": ["shared.py"] }
                ]
            })),
            Role::Worker => {
                let number = spec.id.trim_start_matches("worker-");
                fs::write(folder.join("shared.py"), format!("value = {number}\n")).unwrap();
                None
            }
            Role::Resolver => {
                if let Some(text) = self.resolution {
                    fs::write(folder.join("shared.py"), text).unwrap();
                }
                None
            }
            Role::Reviewer => Some(
                json!({ "verdict": "approve", "summary": "Both are in.", "must_fix": [], "consider": [] }),
            ),
            Role::Dispatcher => None,
        };
        Ok(AgentOutput {
            text: format!("{} done", spec.id),
            structured,
            session_id: Some(format!("s-{}", spec.id)),
        })
    }
}

fn clashing_orchestrator(runner: Arc<ClashingRunner>, resolve: bool) -> Orchestrator {
    let settings = Settings { resolve_conflicts: resolve, ..Settings::default() };
    Orchestrator::new(runner, settings::shared(settings), Isolation::Worktrees)
}

#[tokio::test]
async fn the_resolver_merges_two_workers_changes_to_the_same_lines() {
    let project = project();
    let runner = ClashingRunner::new(Some("value = 1\nalso = 2\n"));
    let orchestrator = clashing_orchestrator(runner.clone(), true);
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;
    assert_eq!(run.state(), RunState::Done);

    // It worked where everything is merged together, not in a worker's folder.
    let sessions = runner.sessions.lock().unwrap().clone();
    let (_, resolver_folder) = sessions.iter().find(|(id, _)| id == "resolver").expect("the resolver ran");
    let (_, reviewer_folder) = sessions.iter().find(|(id, _)| id == "reviewer").expect("the reviewer ran");
    assert_eq!(resolver_folder, reviewer_folder);

    // The round carried on: the reviewer saw the merged file and approved it.
    assert!(collected.of_type("review").iter().any(|event| event["verdict"] == "approve"));

    supervised(run.clone(), orchestrator.apply(&run, Some("both changes"), false)).await;
    assert_eq!(run.state(), RunState::Ready);
    assert_eq!(fs::read_to_string(project.path().join("shared.py")).unwrap(), "value = 1\nalso = 2\n");
    let log = git(project.path(), &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 2, "applying with a message commits it: {log}");
    assert!(log.contains("both changes"));
}

#[tokio::test]
async fn a_resolver_that_leaves_the_markers_in_place_changes_nothing() {
    let project = project();
    let runner = ClashingRunner::new(None);
    let orchestrator = clashing_orchestrator(runner.clone(), true);
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;

    // The round is still yours to decide about, and nothing half-merged was kept.
    assert_eq!(run.state(), RunState::Done);
    assert!(runner.ran("resolver"));
    assert!(!runner.ran("reviewer"), "there was nothing whole to review");
    let problems = collected.of_type("error");
    assert!(problems.iter().any(|event| event["message"].as_str().unwrap().contains("shared.py")));
    assert!(!project.path().join("shared.py").exists(), "the project was not touched");
}

#[tokio::test]
async fn without_the_resolver_a_clash_just_stops_the_round() {
    let project = project();
    let runner = ClashingRunner::new(Some("never used"));
    let orchestrator = clashing_orchestrator(runner.clone(), false);
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;

    assert_eq!(run.state(), RunState::Done);
    assert!(!runner.ran("resolver"), "it is turned off");
    let problems = collected.of_type("error");
    assert!(problems
        .iter()
        .any(|event| event["message"].as_str().unwrap().contains("turned off in Settings")));
}

#[tokio::test]
async fn the_apps_own_check_runs_on_the_merged_work_and_the_reviewer_is_told() {
    let project = project();
    let runner = Arc::new(WritingRunner::default());
    let settings = Settings { check_command: "echo checked-ok".into(), ..Settings::default() };
    let orchestrator = Orchestrator::new(runner.clone(), settings::shared(settings), Isolation::Worktrees);
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;

    let checks = collected.of_type("checks");
    assert_eq!(checks.len(), 1, "once per review");
    assert_eq!(checks[0]["passed"], true);
    assert_eq!(checks[0]["command"], "echo checked-ok");
    assert!(checks[0]["output"].as_str().unwrap().contains("checked-ok"));

    let review_prompt = runner.sessions.lock().unwrap()[3].2.clone();
    assert!(review_prompt.contains("checked-ok"), "the reviewer does not have to run it again");
    assert!(review_prompt.contains("passed"), "{review_prompt}");
}

#[tokio::test]
async fn passing_checks_can_skip_the_review_and_review_again_still_reviews() {
    let reviews = |runner: &WritingRunner| {
        runner.sessions.lock().unwrap().iter().filter(|(id, _, _)| id == "reviewer").count()
    };
    let round = |check: &str| {
        let settings = Settings {
            check_command: check.into(),
            skip_review_when_checks_pass: true,
            ..Settings::default()
        };
        let runner = Arc::new(WritingRunner::default());
        (runner.clone(), Orchestrator::new(runner, settings::shared(settings), Isolation::Worktrees))
    };

    // The checks pass: no reviewer, and the round says so.
    let passing = project();
    let (runner, orchestrator) = round("true");
    let collected = Collected::default();
    let run = new_run(passing.path(), &collected);
    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;
    assert_eq!(reviews(&runner), 0);
    assert_eq!(run.state(), RunState::Done);
    assert!(collected
        .of_type("notice")
        .iter()
        .any(|n| n["text"].as_str().unwrap().contains("review was skipped")));

    // "Review again" is the user asking for one, setting or not.
    supervised(run.clone(), orchestrator.review(&run)).await;
    assert_eq!(reviews(&runner), 1);

    // The checks fail: the reviewer looks, whatever the setting says.
    let failing = project();
    let (runner, orchestrator) = round("false");
    let run = new_run(failing.path(), &Collected::default());
    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;
    assert_eq!(reviews(&runner), 1);
}

#[tokio::test]
async fn a_round_can_be_picked_up_after_the_app_is_restarted() {
    let project = project();
    let runner = Arc::new(WritingRunner::default());
    let orchestrator = orchestrator(runner.clone());
    let collected = Collected::default();
    let run = new_run(project.path(), &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;
    assert_eq!(run.state(), RunState::Done);

    // What the app writes to disk between phases, through JSON and back.
    let saved = run.snapshot();
    let text = serde_json::to_string(&saved).unwrap();
    let read_back: agent_console_core::run::Snapshot = serde_json::from_str(&text).unwrap();
    let events = run.events();
    let request = run.request.clone();
    let id = run.id.clone();
    drop(run);

    // As the app does on opening a saved run: find the round's worktrees again.
    let round = read_back.round.as_ref().unwrap();
    let kept = round.workspace.as_ref().unwrap();
    let workspace =
        agent_console_core::worktrees::Workspace::reopen(project.path(), &id, 1, kept.workers, &kept.base)
            .await
            .unwrap();
    let again = Run::from_saved(
        id,
        project.path(),
        request,
        events,
        Some(read_back),
        Some(Arc::new(workspace)),
        collected.sink(),
    );

    assert!(again.resumable(), "its sessions and worktrees are all still there");
    assert_eq!(again.state(), RunState::Done, "still waiting for you to decide");
    assert_eq!(again.tasks().len(), 2, "the plan came back too");
    assert_eq!(again.session("planner").as_deref(), Some("s-planner"), "the planner can be resumed");

    // And the round really can be finished: the worktrees are the same ones.
    supervised(again.clone(), orchestrator.apply(&again, None, false)).await;
    assert_eq!(again.state(), RunState::Ready);
    let staged = git(project.path(), &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("file1.py") && staged.contains("file2.py"), "staged: {staged}");
}

#[tokio::test]
async fn a_saved_run_with_nothing_kept_about_it_can_only_be_read() {
    let project = project();
    let collected = Collected::default();
    let run = Run::from_saved(
        "old".to_string(),
        project.path(),
        "an older run",
        Vec::new(),
        None,
        None,
        collected.sink(),
    );
    assert!(!run.resumable());
}
