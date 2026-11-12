//! Helpers shared by the integration tests.
#![allow(dead_code)] // each test file uses a different subset

use std::sync::{Arc, Mutex};

use agent_console_core::run::EventSink;
use agent_console_core::{Event, Run};
use serde_json::Value;

/// A sink that keeps every event, as JSON, so tests can look at them afterwards.
#[derive(Clone, Default)]
pub struct Collected(Arc<Mutex<Vec<Value>>>);

impl Collected {
    pub fn sink(&self) -> EventSink {
        let events = self.0.clone();
        Arc::new(move |_run_id: &str, event: &Event| {
            events.lock().unwrap().push(serde_json::to_value(event).unwrap());
        })
    }

    pub fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }

    /// Events with the given `type`.
    pub fn of_type(&self, kind: &str) -> Vec<Value> {
        self.all().into_iter().filter(|event| event["type"] == kind).collect()
    }

    /// The status changes of one agent, in order, e.g. `["running", "done"]`.
    pub fn statuses(&self, agent: &str) -> Vec<String> {
        self.of_type("agent_status")
            .into_iter()
            .filter(|event| event["agent"] == agent)
            .map(|event| event["status"].as_str().unwrap().to_string())
            .collect()
    }
}

pub fn new_run(project_dir: impl Into<std::path::PathBuf>, collected: &Collected) -> Arc<Run> {
    Run::new(project_dir, "add two helpers", collected.sink())
}
