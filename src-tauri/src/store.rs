//! Saving runs so they outlive the app.
//!
//! One file per run, in `runs/` next to the settings, written a line at a time:
//!
//! ```text
//! {"id":"ab12…","project_dir":"/code/app","request":"Add tests","started_at":1762…}
//! {"seq":0,"type":"run_state","state":"ready"}
//! {"seq":1,"agent":"dispatcher","type":"agent_status","status":"running"}
//! ```
//!
//! The first line says what the run is; every line after it is one event, exactly
//! as the window receives it. Appending a line is cheap and a half-written line at
//! the end (the app was killed mid-write) costs nothing: it is skipped on reading.
//!
//! Beside it, `runs/<id>.state.json` holds where the run had got to after its last
//! phase: the agent sessions to resume, the open round, the plan. That is what
//! lets a run be carried on after the app is closed; without it, or with its
//! worktrees gone, the run can still be read.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_console_core::event::EventKind;
use agent_console_core::run::Snapshot;
use agent_console_core::settings::{RoleSettings, Settings};
use agent_console_core::{Event, Run};
use serde::{Deserialize, Serialize};

/// The most runs the Runs window lists, newest first.
const MAX_LISTED: usize = 60;

/// The first line of a run's file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Head {
    pub id: String,
    pub project_dir: String,
    pub request: String,
    /// Seconds since 1970, so the window can show the date in the user's own format.
    pub started_at: u64,
    /// How the team was set up for this run. Missing in runs saved before the app
    /// kept it, which is why the Runs window has to cope with `None`.
    #[serde(default)]
    pub models: Option<RunModels>,
}

/// What a run was set up with, so its cost can be compared with another's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunModels {
    /// The preset this matches ("lean", "balanced", "thorough"), when it matches one.
    pub preset: Option<String>,
    pub dispatcher: RoleSettings,
    pub planner: RoleSettings,
    pub worker: RoleSettings,
    pub reviewer: RoleSettings,
    /// Most workers one plan could use.
    pub workers: usize,
}

impl RunModels {
    pub fn of(settings: &Settings) -> RunModels {
        RunModels {
            preset: settings
                .matching_preset()
                .and_then(|preset| serde_json::to_value(preset).ok())
                .and_then(|value| value.as_str().map(str::to_string)),
            dispatcher: settings.dispatcher.clone(),
            planner: settings.planner.clone(),
            worker: settings.worker.clone(),
            reviewer: settings.reviewer.clone(),
            workers: settings.worker_count,
        }
    }
}

/// One run in the Runs window.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: String,
    pub project_dir: String,
    pub request: String,
    pub started_at: u64,
    pub state: String,
    pub rounds: usize,
    pub tokens: u64,
    pub cost_usd: f64,
    pub models: Option<RunModels>,
}

pub struct RunStore {
    folder: PathBuf,
}

impl RunStore {
    /// Runs are kept in `runs/` inside the app's own folder.
    pub fn new(app_folder: &Path) -> RunStore {
        RunStore { folder: app_folder.join("runs") }
    }

    fn path(&self, run_id: &str) -> PathBuf {
        self.folder.join(format!("{run_id}.jsonl"))
    }

    fn state_path(&self, run_id: &str) -> PathBuf {
        self.folder.join(format!("{run_id}.state.json"))
    }

    /// Write the first line of a new run's file.
    pub fn start(&self, run: &Run, models: RunModels) {
        let head = Head {
            id: run.id.clone(),
            project_dir: run.project_dir.to_string_lossy().into_owned(),
            request: run.request.clone(),
            started_at: now(),
            models: Some(models),
        };
        if fs::create_dir_all(&self.folder).is_err() {
            return;
        }
        if let Ok(line) = serde_json::to_string(&head) {
            let _ = fs::write(self.path(&run.id), line + "\n");
        }
    }

    /// Add one event to a run's file. Anything that goes wrong here is ignored:
    /// the run itself matters more than the record of it.
    pub fn append(&self, run_id: &str, event: &Event) {
        let Ok(line) = serde_json::to_string(event) else { return };
        let opened = OpenOptions::new().append(true).open(self.path(run_id));
        if let Ok(mut file) = opened {
            let _ = file.write_all((line + "\n").as_bytes());
        }
    }

    /// Save where the run has got to. Called after each phase, so that the app
    /// closing, or being force-quit, costs at most the phase in progress.
    pub fn save_state(&self, run: &Run) {
        let Ok(text) = serde_json::to_string(&run.snapshot()) else { return };
        let path = self.state_path(&run.id);
        let temporary = path.with_extension("tmp");
        // Written beside it and renamed, so a file that exists is always whole.
        if fs::write(&temporary, text).is_ok() {
            let _ = fs::rename(&temporary, &path);
        }
    }

    /// Where a saved run had got to, if that was kept.
    pub fn read_state(&self, run_id: &str) -> Option<Snapshot> {
        let text = fs::read_to_string(self.state_path(run_id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Every saved run, newest first.
    pub fn list(&self) -> Vec<RunSummary> {
        let Ok(entries) = fs::read_dir(&self.folder) else { return Vec::new() };
        let mut files: Vec<(u64, PathBuf)> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|kind| kind == "jsonl"))
            .map(|path| (modified(&path), path))
            .collect();
        // Newest first: the time is negated so the plain sort puts the largest first.
        files.sort_by_key(|(changed, _)| std::cmp::Reverse(*changed));
        files.iter().take(MAX_LISTED).filter_map(|(_, path)| summarise(path)).collect()
    }

    /// Read a saved run back: what it was, and everything that happened in it.
    pub fn read(&self, run_id: &str) -> Result<(Head, Vec<Event>), String> {
        let (head, events) = read_file(&self.path(run_id))
            .ok_or("That run is not among the saved ones, or its file cannot be read.")?;
        Ok((head, events))
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|since| since.as_secs()).unwrap_or_default()
}

fn modified(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|data| data.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

/// Read one run's file. A line that does not parse is skipped, so a file cut
/// short by a crash still gives back everything before the cut.
fn read_file(path: &Path) -> Option<(Head, Vec<Event>)> {
    let file = File::open(path).ok()?;
    let mut lines = BufReader::new(file).lines().map_while(Result::ok);
    let head: Head = serde_json::from_str(&lines.next()?).ok()?;
    let events = lines.filter_map(|line| serde_json::from_str::<Event>(&line).ok()).collect();
    Some((head, events))
}

/// What the Runs window shows for one run, worked out from its events.
fn summarise(path: &Path) -> Option<RunSummary> {
    let (head, events) = read_file(path)?;
    let mut summary = RunSummary {
        id: head.id,
        project_dir: head.project_dir,
        request: head.request,
        started_at: head.started_at,
        state: "ready".to_string(),
        rounds: 0,
        tokens: 0,
        cost_usd: 0.0,
        models: head.models,
    };
    for event in &events {
        match &event.kind {
            EventKind::RunState { state } => {
                summary.state = serde_json::to_value(state)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| summary.state.clone());
            }
            EventKind::Round { open: true, .. } => summary.rounds += 1,
            EventKind::Result { tokens, cost_usd, .. } => {
                summary.tokens += tokens.input_tokens
                    + tokens.output_tokens
                    + tokens.cache_read_input_tokens
                    + tokens.cache_creation_input_tokens;
                summary.cost_usd += cost_usd.unwrap_or(0.0);
            }
            _ => {}
        }
    }
    Some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_console_core::event::TokenUsage;
    use agent_console_core::run::RunState;
    use std::sync::Arc;

    fn quiet_run(project: &str, request: &str) -> Arc<Run> {
        Run::new(project, request, Arc::new(|_: &str, _: &Event| {}))
    }

    fn models() -> RunModels {
        RunModels::of(&Settings::default())
    }

    #[test]
    fn a_run_is_written_line_by_line_and_read_back_whole() {
        let folder = tempfile::tempdir().unwrap();
        let store = RunStore::new(folder.path());
        let run = quiet_run("/code/app", "Add tests");
        store.start(&run, models());

        let events = [
            Event { seq: 0, agent: None, kind: EventKind::RunState { state: RunState::Planning } },
            Event { seq: 1, agent: None, kind: EventKind::Round { open: true, quick: false } },
            Event {
                seq: 2,
                agent: Some("worker-1".to_string()),
                kind: EventKind::Result {
                    cost_usd: Some(0.25),
                    turns: Some(3),
                    tokens: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                        cache_read_input_tokens: 900,
                        cache_creation_input_tokens: 0,
                    },
                },
            },
            Event { seq: 3, agent: None, kind: EventKind::RunState { state: RunState::Done } },
        ];
        for event in &events {
            store.append(&run.id, event);
        }

        let (head, read_back) = store.read(&run.id).unwrap();
        assert_eq!(head.request, "Add tests");
        assert_eq!(head.project_dir, "/code/app");
        assert_eq!(read_back.len(), 4);
        assert!(matches!(read_back[1].kind, EventKind::Round { open: true, .. }));

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        let how = listed[0].models.as_ref().expect("how the run was set up is kept");
        assert_eq!(how.preset.as_deref(), Some("balanced"));
        assert_eq!(how.planner.model, "opus");
        assert_eq!(listed[0].state, "done", "the last state it reached");
        assert_eq!(listed[0].rounds, 1);
        assert_eq!(listed[0].tokens, 1050);
        assert!((listed[0].cost_usd - 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_file_cut_short_still_gives_back_what_it_has() {
        let folder = tempfile::tempdir().unwrap();
        let store = RunStore::new(folder.path());
        let run = quiet_run("/code/app", "Half a run");
        store.start(&run, models());
        store.append(
            &run.id,
            &Event { seq: 0, agent: None, kind: EventKind::Round { open: true, quick: false } },
        );
        // As if the app were killed in the middle of writing a line.
        let mut file = OpenOptions::new().append(true).open(store.path(&run.id)).unwrap();
        file.write_all(b"{\"seq\":1,\"type\":\"ru").unwrap();

        let (head, events) = store.read(&run.id).unwrap();
        assert_eq!(head.request, "Half a run");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn where_a_run_got_to_is_saved_beside_it() {
        let folder = tempfile::tempdir().unwrap();
        let store = RunStore::new(folder.path());
        let run = quiet_run("/code/app", "Add tests");
        store.start(&run, models());
        assert!(store.read_state(&run.id).is_none(), "nothing saved yet");

        store.save_state(&run);
        let state = store.read_state(&run.id).expect("saved after a phase");
        assert!(state.round.is_none());
        assert_eq!(state.rounds_started, 0);
        // The run's own file is untouched by this.
        assert_eq!(store.read(&run.id).unwrap().1.len(), 0);
    }

    #[test]
    fn runs_are_listed_newest_first_and_an_unknown_one_says_so() {
        let folder = tempfile::tempdir().unwrap();
        let store = RunStore::new(folder.path());
        for request in ["first", "second"] {
            let run = quiet_run("/code/app", request);
            store.start(&run, models());
            // Files a second apart, so the order is not down to chance.
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        let listed = store.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].request, "second");
        assert!(store.read("nothing-like-this").is_err());
    }
}
