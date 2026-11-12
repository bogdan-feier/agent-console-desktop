//! The whole plan -> build -> review flow, using the scripted demo agents.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_console_core::config::{Task, Verdict};
use agent_console_core::demo::DemoRunner;
use agent_console_core::orchestrator::supervised;
use agent_console_core::{Orchestrator, RunState};
use common::{new_run, Collected};

fn instant_demo() -> Orchestrator {
    Orchestrator::new(Arc::new(DemoRunner::new(Duration::ZERO)))
}

#[tokio::test]
async fn planning_ends_waiting_for_approval() {
    let collected = Collected::default();
    let run = new_run(".", &collected);
    let orchestrator = instant_demo();

    supervised(run.clone(), orchestrator.plan(&run)).await;

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

    supervised(run.clone(), orchestrator.plan(&run)).await;
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

    supervised(run.clone(), orchestrator.plan(&run)).await;
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

    supervised(run.clone(), orchestrator.plan(&run)).await;
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
    let orchestrator = Orchestrator::new(Arc::new(DemoRunner::new(Duration::from_secs(30))));

    let job = {
        let run = run.clone();
        tokio::spawn(async move { supervised(run.clone(), orchestrator.plan(&run)).await })
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
