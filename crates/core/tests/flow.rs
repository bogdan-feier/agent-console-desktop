//! The whole flow with scripted agents: the dispatcher, planning, a round with three
//! workers and one review, sending work back, applying and discarding. (Scripted
//! agents touch no files, so these use the shared-folder mode; the real worktrees
//! are tested in `worktree_round.rs`.)

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_console_core::config::{AgentSpec, Task, Verdict};
use agent_console_core::orchestrator::{
    can_approve, can_chat, can_finish_round, can_send_back, supervised, Isolation,
};
use agent_console_core::settings::{self, Settings};
use agent_console_core::{AgentError, AgentOutput, AgentRunner, Orchestrator, Run, RunState};
use common::scripted::ScriptedRunner;
use common::{new_run, Collected};

fn instant_script() -> Orchestrator {
    Orchestrator::new(
        Arc::new(ScriptedRunner::new(Duration::ZERO)),
        settings::shared(Settings::default()),
        Isolation::SharedFolder,
    )
}

/// A run that has been planned, built and reviewed once.
async fn reviewed_run(collected: &Collected) -> (Arc<agent_console_core::Run>, Orchestrator) {
    let run = new_run(".", collected);
    let orchestrator = instant_script();
    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;
    (run, orchestrator)
}

#[tokio::test]
async fn dispatcher_answers_quick_questions_itself() {
    let collected = Collected::default();
    let run = new_run(".", &collected);

    supervised(run.clone(), instant_script().ask(&run, "What does stats.py do?")).await;

    assert_eq!(run.state(), RunState::Ready);
    assert_eq!(collected.statuses("dispatcher"), ["running", "done"]);
    assert!(collected.statuses("planner").is_empty(), "the planner was not needed");
    let answer = collected.of_type("text").last().unwrap()["text"].as_str().unwrap().to_string();
    assert!(answer.contains("count_words"), "{answer}");
}

#[tokio::test]
async fn a_plan_with_no_tasks_is_an_answer_not_a_failure() {
    let collected = Collected::default();
    let run = new_run(".", &collected);

    supervised(run.clone(), instant_script().plan(&run, Some("ok, how do I run the app now?"))).await;

    assert_eq!(run.state(), RunState::Ready, "the run carries on");
    assert!(collected.of_type("error").is_empty(), "no error is shown");
    assert!(collected.of_type("plan").is_empty(), "no empty plan card");
    let answer = collected.of_type("text").last().unwrap()["text"].as_str().unwrap().to_string();
    assert!(answer.contains("uv run textstats"), "{answer}");
}

#[tokio::test]
async fn dispatcher_passes_ideas_to_the_planner_to_talk_through() {
    let collected = Collected::default();
    let run = new_run(".", &collected);

    supervised(run.clone(), instant_script().ask(&run, "Thoughts on better text statistics")).await;

    assert_eq!(run.state(), RunState::Ready);
    assert_eq!(collected.statuses("planner"), ["running", "done"]);
    assert_eq!(run.session("planner").as_deref(), Some("scripted-planner"));
    // The user's message appears in both panels: the dispatcher's, and the planner's.
    let to: Vec<_> = collected.of_type("user_message").iter().map(|e| e["agent"].clone()).collect();
    assert_eq!(to, ["dispatcher", "planner"]);
}

#[tokio::test]
async fn dispatcher_passes_clear_requests_on_to_be_planned() {
    let collected = Collected::default();
    let run = new_run(".", &collected);

    supervised(run.clone(), instant_script().ask(&run, "Add three text helpers")).await;

    assert_eq!(run.state(), RunState::AwaitingApproval);
    assert_eq!(run.tasks().len(), 3);
    assert_eq!(collected.of_type("plan").len(), 1);
}

#[tokio::test]
async fn dispatcher_sends_small_clear_changes_straight_to_a_worker() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_script();

    supervised(run.clone(), orchestrator.ask(&run, "Fix the typo in the greeting")).await;
    assert_eq!(collected.statuses("worker-1"), ["running", "done"]);
    assert!(collected.statuses("planner").is_empty(), "no plan for a quick change");
    assert!(run.review().is_none(), "and no review");

    // With that round still open, another quick change cannot open one, so it is planned.
    supervised(run.clone(), orchestrator.ask(&run, "Fix another typo")).await;
    assert_eq!(collected.statuses("planner"), ["running", "done"]);
    assert_eq!(run.state(), RunState::AwaitingApproval);
    let notices = collected.of_type("notice");
    assert!(notices
        .iter()
        .any(|notice| notice["text"].as_str().unwrap().contains("planning this one instead")));
}

#[tokio::test]
async fn a_quick_change_runs_one_worker_and_nobody_else() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_script();

    supervised(run.clone(), orchestrator.quick(&run, "Add a farewell helper")).await;

    assert_eq!(run.state(), RunState::Done);
    assert!(run.has_open_round(), "the changes wait for the user, as after a full round");
    assert_eq!(collected.statuses("worker-1"), ["running", "done"]);
    for skipped in ["planner", "reviewer", "worker-2", "dispatcher"] {
        assert!(collected.statuses(skipped).is_empty(), "{skipped} should not have run");
    }
    assert!(run.review().is_none(), "nothing was reviewed");
}

#[tokio::test]
async fn a_quick_change_admits_that_nobody_reviewed_it() {
    let collected = Collected::default();
    let run = new_run(".", &collected);

    supervised(run.clone(), instant_script().quick(&run, "Add a farewell helper")).await;

    let said = collected.of_type("notice");
    assert!(
        said.iter().any(|notice| notice["text"].as_str().unwrap_or("").contains("Nobody reviewed")),
        "the user has to be told what they are not getting: {said:?}"
    );
}

#[tokio::test]
async fn a_round_runs_three_workers_and_one_review() {
    let collected = Collected::default();
    let (run, _) = reviewed_run(&collected).await;

    assert_eq!(run.state(), RunState::Done);
    assert!(run.has_open_round());
    for agent in ["planner", "worker-1", "worker-2", "worker-3", "reviewer"] {
        assert_eq!(collected.statuses(agent), ["running", "done"], "{agent}");
    }
    let review = run.review().unwrap();
    assert_eq!(review.verdict, Verdict::NeedsChanges);
    assert_eq!(review.findings_for(2).len(), 1);
    assert_eq!(collected.of_type("round")[0]["open"], true);
}

#[tokio::test]
async fn findings_no_task_covered_can_be_given_to_a_worker() {
    let collected = Collected::default();
    let (run, orchestrator) = reviewed_run(&collected).await;
    assert_eq!(run.review().unwrap().for_the_user().len(), 1);
    assert!(can_send_back(&run, 0).is_ok(), "the findings for nobody can still be acted on");

    supervised(run.clone(), orchestrator.send_back(&run, 0)).await;

    // The first worker that finished took them, and the reviewer looked again.
    assert_eq!(collected.statuses("worker-1"), ["running", "done", "running", "done"]);
    assert_eq!(collected.statuses("worker-2"), ["running", "done"], "worker 2 keeps its own finding");
    assert_eq!(collected.statuses("reviewer"), ["running", "done", "running", "done"]);
    let notices = collected.of_type("notice");
    assert!(notices
        .iter()
        .any(|n| n["agent"] == "worker-1" && n["text"].as_str().unwrap().contains("no task covered")));
}

#[tokio::test]
async fn sending_work_back_resumes_that_worker_and_reviews_again() {
    let collected = Collected::default();
    let (run, orchestrator) = reviewed_run(&collected).await;
    assert!(can_send_back(&run, 2).is_ok());
    assert!(can_send_back(&run, 1).is_err(), "nothing for worker 1 to fix");

    supervised(run.clone(), orchestrator.send_back(&run, 2)).await;

    assert_eq!(run.state(), RunState::Done);
    assert_eq!(run.review().unwrap().verdict, Verdict::Approve);
    assert!(run.worker_report(2).unwrap().contains("Fixed"));
    assert_eq!(collected.statuses("worker-2"), ["running", "done", "running", "done"]);
    assert_eq!(collected.statuses("worker-1"), ["running", "done"], "worker 1 was not touched");
    assert_eq!(collected.statuses("reviewer"), ["running", "done", "running", "done"]);
    let notices = collected.of_type("notice");
    assert!(
        notices
            .iter()
            .any(|n| n["agent"] == "worker-2" && n["text"].as_str().unwrap().starts_with("Sent back")),
        "the worker's card says why it started again"
    );
}

#[tokio::test]
async fn applying_closes_the_round_and_ends_the_workers_sessions() {
    let collected = Collected::default();
    let (run, orchestrator) = reviewed_run(&collected).await;
    assert!(can_approve(&run).is_err(), "a new plan waits until this round is finished");
    assert!(can_chat(&run, "worker-1", "hi").is_ok());

    supervised(run.clone(), orchestrator.apply(&run, None, false)).await;

    assert_eq!(run.state(), RunState::Ready);
    assert!(!run.has_open_round());
    assert!(can_chat(&run, "worker-1", "hi").is_err());
    assert!(can_chat(&run, "planner", "hi").is_ok(), "the planner remembers across rounds");
    assert!(can_finish_round(&run).is_err());
    assert_eq!(collected.of_type("round").last().unwrap()["open"], false);
}

#[tokio::test]
async fn discarding_closes_the_round() {
    let collected = Collected::default();
    let (run, orchestrator) = reviewed_run(&collected).await;

    supervised(run.clone(), orchestrator.discard(&run)).await;

    assert_eq!(run.state(), RunState::Ready);
    assert!(!run.has_open_round());
    let notice = collected.of_type("notice").last().unwrap()["text"].as_str().unwrap().to_string();
    assert!(notice.contains("Discarded"));
}

#[tokio::test]
async fn who_can_be_talked_to_and_when() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_script();

    assert!(can_chat(&run, "planner", "hello").is_ok());
    assert!(can_chat(&run, "planner", "   ").is_err());
    assert!(can_chat(&run, "dispatcher", "hello").is_err(), "The dispatcher is reached through the main box");
    assert!(can_chat(&run, "worker-1", "hello").is_err(), "worker has not worked yet");
    assert!(can_chat(&run, "reviewer", "hello").is_err());

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;
    assert!(can_chat(&run, "worker-3", "hello").is_ok());
    // Not even after it reviewed: its verdict is final, and "Review again" asks again.
    assert!(can_chat(&run, "reviewer", "why?").is_err());
    let too_long = "x".repeat(agent_console_core::orchestrator::MAX_MESSAGE_CHARS + 1);
    assert!(can_chat(&run, "planner", &too_long).is_err());
}

#[tokio::test]
async fn talking_to_a_worker_updates_its_report_for_the_next_review() {
    let collected = Collected::default();
    let (run, orchestrator) = reviewed_run(&collected).await;

    supervised(run.clone(), orchestrator.chat(&run, "worker-1", "Rename the function to longest()")).await;
    assert_eq!(run.state(), RunState::Done);
    assert!(run.worker_report(1).unwrap().contains("Rename the function"));

    supervised(run.clone(), orchestrator.review(&run)).await;
    assert_eq!(run.review().unwrap().verdict, Verdict::Approve);
}

#[tokio::test]
async fn chatting_while_a_plan_waits_keeps_it_waiting() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_script();

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.chat(&run, "planner", "Why three tasks?")).await;

    assert_eq!(run.state(), RunState::AwaitingApproval);
    let states: Vec<_> = collected.of_type("run_state").iter().map(|e| e["state"].clone()).collect();
    assert_eq!(states, ["planning", "awaiting_approval", "chatting", "awaiting_approval"]);
}

#[tokio::test]
async fn a_single_task_uses_one_worker() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let task = Task {
        title: "Only task".to_string(),
        instructions: "Do it".to_string(),
        files: vec!["a.py".to_string()],
        context: String::new(),
        hard: false,
    };

    supervised(run.clone(), instant_script().build(&run, vec![task])).await;

    assert_eq!(run.state(), RunState::Done);
    assert_eq!(collected.statuses("worker-1"), ["running", "done"]);
    assert!(collected.statuses("worker-2").is_empty());
    assert_eq!(collected.statuses("reviewer"), ["running", "done"]);
}

#[tokio::test]
async fn events_are_numbered_in_order() {
    let collected = Collected::default();
    let (run, _) = reviewed_run(&collected).await;

    let seen: Vec<u64> = collected.all().iter().map(|event| event["seq"].as_u64().unwrap()).collect();
    let expected: Vec<u64> = (0..seen.len() as u64).collect();
    assert_eq!(seen, expected);
    assert_eq!(run.events().len(), seen.len());
}

#[tokio::test]
async fn changed_settings_apply_to_the_next_agent() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let shared = settings::shared(Settings::default());
    let orchestrator = Orchestrator::new(
        Arc::new(ScriptedRunner::new(Duration::ZERO)),
        shared.clone(),
        Isolation::SharedFolder,
    );

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    shared.write().unwrap().worker.model = "haiku".to_string();
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;

    let model_of = |agent: &str| {
        collected.of_type("agent_model").into_iter().find(|e| e["agent"] == agent).unwrap()["model"].clone()
    };
    assert_eq!(model_of("planner"), "scripted-opus");
    assert_eq!(model_of("worker-1"), "scripted-haiku");
    assert_eq!(model_of("reviewer"), "scripted-sonnet");
}

#[tokio::test]
async fn stopping_marks_running_agents_as_stopped() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    // Slow steps, so the planner is still working when we stop it.
    let orchestrator = Orchestrator::new(
        Arc::new(ScriptedRunner::new(Duration::from_secs(30))),
        settings::shared(Settings::default()),
        Isolation::SharedFolder,
    );

    let job = {
        let run = run.clone();
        tokio::spawn(async move { supervised(run.clone(), orchestrator.plan(&run, None)).await })
    };
    while !run.is_busy() || collected.statuses("planner").is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(run.stop());
    job.await.unwrap();

    assert_eq!(run.state(), RunState::Stopped);
    assert_eq!(collected.statuses("planner"), ["running", "stopped"]);
    assert!(!run.stop(), "nothing left to stop");
}

/// Wraps the scripted runner to record when each agent starts, with a start gap.
struct GappedRunner {
    inner: ScriptedRunner,
    gap: Duration,
    starts: std::sync::Mutex<Vec<(String, std::time::Instant)>>,
}

#[async_trait::async_trait]
impl agent_console_core::AgentRunner for GappedRunner {
    fn parallel_start_gap(&self) -> Duration {
        self.gap
    }

    async fn run_session(
        &self,
        run: &agent_console_core::Run,
        spec: &agent_console_core::config::AgentSpec,
        prompt: &str,
        resume: Option<&str>,
        folder: &std::path::Path,
    ) -> Result<agent_console_core::AgentOutput, agent_console_core::AgentError> {
        self.starts.lock().unwrap().push((spec.id.clone(), std::time::Instant::now()));
        self.inner.run_session(run, spec, prompt, resume, folder).await
    }
}

#[tokio::test]
async fn parallel_workers_start_one_gap_apart() {
    let gap = Duration::from_millis(200);
    let runner = Arc::new(GappedRunner {
        inner: ScriptedRunner::new(Duration::ZERO),
        gap,
        starts: Default::default(),
    });
    let orchestrator =
        Orchestrator::new(runner.clone(), settings::shared(Settings::default()), Isolation::SharedFolder);
    let run = new_run(".", &Collected::default());

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build(&run, run.tasks())).await;

    let starts = runner.starts.lock().unwrap().clone();
    let at = |agent: &str| starts.iter().find(|(id, _)| id == agent).map(|(_, at)| *at).unwrap();
    // Each wait is measured from the same moment, and the first worker itself starts a
    // millisecond or so after that, so allow a little slack.
    let at_least = gap - Duration::from_millis(20);
    assert!(at("worker-2") - at("worker-1") >= at_least);
    assert!(at("worker-3") - at("worker-2") >= at_least);
}

/// A runner whose planner answers with prose where a plan was asked for, which a
/// real model does now and then.
struct RamblingRunner;

#[async_trait::async_trait]
impl AgentRunner for RamblingRunner {
    async fn run_session(
        &self,
        _run: &Run,
        _spec: &AgentSpec,
        _prompt: &str,
        _resume: Option<&str>,
        _folder: &Path,
    ) -> Result<AgentOutput, AgentError> {
        Ok(AgentOutput {
            text: "I had a think about it and I would start with the parser.".to_string(),
            structured: None,
            session_id: Some("s".to_string()),
        })
    }
}

#[tokio::test]
async fn an_agent_that_answers_with_prose_instead_of_a_plan_fails_readably() {
    let orchestrator = Orchestrator::new(
        Arc::new(RamblingRunner),
        settings::shared(Settings::default()),
        Isolation::SharedFolder,
    );
    let collected = Collected::default();
    let run = new_run("/tmp", &collected);

    supervised(run.clone(), orchestrator.plan(&run, None)).await;

    assert_eq!(run.state(), RunState::Failed, "it stops rather than carrying on with nothing");
    let problems = collected.of_type("error");
    assert!(
        problems.iter().any(|event| event["message"].as_str().unwrap().contains("structured answer")),
        "the user is told what went wrong: {problems:?}"
    );
}

#[tokio::test]
async fn a_phase_that_crashes_leaves_the_run_able_to_act_again() {
    let collected = Collected::default();
    let run = new_run("/tmp", &collected);

    // What `spawn_phase` does when the task running a phase panicked.
    run.phase_crashed("Something went wrong inside the app and that step stopped.");

    assert_eq!(run.state(), RunState::Failed);
    assert!(run.is_at_rest(), "the window is not left waiting for a phase that died");
    let problems = collected.of_type("error");
    assert_eq!(problems.len(), 1);
}
