//! The whole plan -> build -> review flow, using the scripted demo agents.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_console_core::config::{Task, Verdict};
use agent_console_core::demo::DemoRunner;
use agent_console_core::orchestrator::{can_chat, supervised};
use agent_console_core::settings::{self, Settings};
use agent_console_core::{Orchestrator, RunState};
use common::{new_run, Collected};

fn instant_demo() -> Orchestrator {
    Orchestrator::new(Arc::new(DemoRunner::new(Duration::ZERO)), settings::shared(Settings::default()))
}

#[tokio::test]
async fn planning_ends_waiting_for_approval() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    supervised(run.clone(), orchestrator.plan(&run, None)).await;

    assert_eq!(run.state(), RunState::AwaitingApproval);
    assert_eq!(run.tasks().len(), 2);
    assert_eq!(collected.of_type("plan").len(), 1);
    assert!(!run.is_busy());
}

#[tokio::test]
async fn build_and_review_runs_every_agent() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build_and_review(&run, run.tasks())).await;

    assert_eq!(run.state(), RunState::Done);
    for agent in ["planner", "worker-1", "worker-2", "reviewer-1", "reviewer-2"] {
        assert_eq!(collected.statuses(agent), ["running", "done"], "{agent}");
    }
    assert_eq!(run.review(1).unwrap().verdict, Verdict::Approve);
    assert_eq!(run.review(2).unwrap().verdict, Verdict::NeedsChanges);
}

#[tokio::test]
async fn events_are_numbered_in_order() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build_and_review(&run, run.tasks())).await;

    let seen: Vec<u64> = collected.all().iter().map(|event| event["seq"].as_u64().unwrap()).collect();
    let expected: Vec<u64> = (0..seen.len() as u64).collect();
    assert_eq!(seen, expected);
    assert_eq!(run.events().len(), seen.len());
}

#[tokio::test]
async fn sending_a_review_back_resumes_the_same_worker() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build_and_review(&run, run.tasks())).await;
    supervised(run.clone(), orchestrator.fix(&run, 2)).await;

    assert_eq!(run.state(), RunState::Done);
    assert_eq!(run.review(2).unwrap().verdict, Verdict::Approve);
    assert!(run.worker_report(2).unwrap().contains("Fixed"));
    assert_eq!(collected.statuses("worker-2"), ["running", "done", "running", "done"]);
    // Worker 1 was not touched by the fix.
    assert_eq!(collected.statuses("worker-1"), ["running", "done"]);
}

#[tokio::test]
async fn a_single_task_uses_one_worker_and_one_reviewer() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let task = Task {
        title: "Only task".to_string(),
        instructions: "Do it".to_string(),
        files: vec!["a.py".to_string()],
    };

    supervised(run.clone(), instant_demo().build_and_review(&run, vec![task])).await;

    assert_eq!(run.state(), RunState::Done);
    assert_eq!(collected.statuses("worker-1"), ["running", "done"]);
    assert!(collected.statuses("worker-2").is_empty());
    assert!(collected.statuses("reviewer-2").is_empty());
}

#[tokio::test]
async fn stopping_marks_running_agents_as_stopped() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    // Slow steps, so the planner is still working when we stop it.
    let orchestrator = Orchestrator::new(
        Arc::new(DemoRunner::new(Duration::from_secs(30))),
        settings::shared(Settings::default()),
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

#[tokio::test]
async fn brainstorming_then_planning_continues_the_planner_session() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();
    assert_eq!(run.state(), RunState::Ready);

    supervised(run.clone(), orchestrator.chat(&run, "planner", "How should we split this?")).await;
    // A chat returns the run to the state it was in.
    assert_eq!(run.state(), RunState::Ready);
    assert_eq!(collected.of_type("user_message")[0]["text"], "How should we split this?");
    assert!(collected
        .of_type("text")
        .iter()
        .any(|e| e["text"].as_str().unwrap().contains("Which do you prefer")));
    assert_eq!(run.session("planner").as_deref(), Some("demo-planner"));

    supervised(run.clone(), orchestrator.plan(&run, Some("Keep each helper in its own file"))).await;
    assert_eq!(run.state(), RunState::AwaitingApproval);
    // The extra direction shows up in the planner's panel as a user message.
    assert_eq!(collected.of_type("user_message").len(), 2);
}

#[tokio::test]
async fn chatting_while_a_plan_waits_keeps_it_waiting() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.chat(&run, "planner", "Why two tasks?")).await;

    assert_eq!(run.state(), RunState::AwaitingApproval);
    let states: Vec<_> = collected.of_type("run_state").iter().map(|e| e["state"].clone()).collect();
    assert_eq!(states, ["planning", "awaiting_approval", "chatting", "awaiting_approval"]);
}

#[tokio::test]
async fn talking_to_a_worker_after_the_build_updates_its_report() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build_and_review(&run, run.tasks())).await;
    supervised(run.clone(), orchestrator.chat(&run, "worker-1", "Rename the function to longest()")).await;

    assert_eq!(run.state(), RunState::Done);
    assert!(run.worker_report(1).unwrap().contains("Rename the function"));
    assert_eq!(collected.statuses("worker-1"), ["running", "done", "running", "done"]);
}

#[tokio::test]
async fn who_can_be_talked_to_and_when() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    assert!(can_chat(&run, "planner", "hello").is_ok());
    assert!(can_chat(&run, "planner", "   ").is_err());
    assert!(can_chat(&run, "worker-1", "hello").is_err(), "worker has not worked yet");
    assert!(can_chat(&run, "boss", "hello").is_err());

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    supervised(run.clone(), orchestrator.build_and_review(&run, run.tasks())).await;
    assert!(can_chat(&run, "worker-1", "hello").is_ok());
    assert!(can_chat(&run, "reviewer-2", "why?").is_ok());
    let too_long = "x".repeat(agent_console_core::orchestrator::MAX_MESSAGE_CHARS + 1);
    assert!(can_chat(&run, "planner", &too_long).is_err());
}

#[tokio::test]
async fn changed_settings_apply_to_the_next_agent() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let shared = settings::shared(Settings::default());
    let orchestrator = Orchestrator::new(Arc::new(DemoRunner::new(Duration::ZERO)), shared.clone());

    supervised(run.clone(), orchestrator.plan(&run, None)).await;
    shared.write().unwrap().worker.model = "haiku".to_string();
    supervised(run.clone(), orchestrator.build_and_review(&run, run.tasks())).await;

    let models: Vec<_> = collected
        .of_type("agent_model")
        .into_iter()
        .map(|e| (e["agent"].as_str().unwrap().to_string(), e["model"].as_str().unwrap().to_string()))
        .collect();
    assert!(models.contains(&("planner".to_string(), "demo-opus".to_string())));
    assert!(models.contains(&("worker-1".to_string(), "demo-haiku".to_string())));
    assert!(models.contains(&("reviewer-1".to_string(), "demo-sonnet".to_string())));
}
