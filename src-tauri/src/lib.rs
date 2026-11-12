//! The desktop app: a thin layer between the page in the window and the engine
//! in `crates/core`.
//!
//! The page calls the `#[tauri::command]` functions below with `invoke(...)`,
//! and receives progress as `run-event` events. Nothing in here knows how
//! agents work; that is all in the core crate.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_console_core::claude::{locate, ClaudeRunner};
use agent_console_core::config::{self, AuthMode, Role, Task};
use agent_console_core::demo::DemoRunner;
use agent_console_core::orchestrator::supervised;
use agent_console_core::run::EventSink;
use agent_console_core::{AgentRunner, Event, Orchestrator, Run, RunState};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{oneshot, OnceCell};

const RUN_EVENT: &str = "run-event";
const DEMO_VARIABLE: &str = "AGENT_CONSOLE_DEMO";
const DEMO_STEP_DELAY: Duration = Duration::from_millis(700);

/// Commands return `Err(String)` for anything the user should read.
type CommandResult<T> = Result<T, String>;

/// One run plus the orchestrator (real or demo) that drives it.
struct ActiveRun {
    run: Arc<Run>,
    orchestrator: Orchestrator,
}

/// Shared by every command. Tauri hands it out as `State<AppState>`.
#[derive(Default)]
struct AppState {
    runs: Mutex<HashMap<String, ActiveRun>>,
    /// Finding `claude` starts a shell, so it is done once, on first use.
    claude: OnceCell<Result<Arc<ClaudeRunner>, String>>,
}

impl AppState {
    fn find(&self, run_id: &str) -> CommandResult<(Arc<Run>, Orchestrator)> {
        let runs = self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = runs.get(run_id).ok_or("That run no longer exists. Start a new one.")?;
        Ok((active.run.clone(), active.orchestrator.clone()))
    }

    async fn claude_runner(&self) -> CommandResult<Arc<ClaudeRunner>> {
        let located = self
            .claude
            .get_or_init(|| async {
                // `locate` blocks while the shell runs, so keep it off the async threads.
                let location = tokio::task::spawn_blocking(locate::locate)
                    .await
                    .map_err(|error| error.to_string())??;
                Ok(Arc::new(ClaudeRunner::new(location, config::AUTH)))
            })
            .await;
        located.clone()
    }
}

#[derive(Serialize)]
struct AgentInfo {
    id: String,
    role: Role,
    label: String,
    model: &'static str,
}

#[derive(Serialize)]
struct AppInfo {
    agents: Vec<AgentInfo>,
    auth: AuthMode,
    /// True when the app was started with AGENT_CONSOLE_DEMO=1.
    demo_default: bool,
}

/// What the page receives for every event: which run it belongs to, and the event.
#[derive(Clone, Serialize)]
struct RunEvent<'a> {
    run_id: &'a str,
    event: &'a Event,
}

#[tauri::command]
fn app_info() -> AppInfo {
    let agents = config::all_agents()
        .into_iter()
        .map(|spec| AgentInfo { id: spec.id, role: spec.role, label: spec.label, model: spec.model })
        .collect();
    AppInfo {
        agents,
        auth: config::AUTH,
        demo_default: env::var(DEMO_VARIABLE).is_ok_and(|value| value == "1"),
    }
}

/// Show the native "choose a folder" dialog.
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    let (sender, receiver) = oneshot::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = sender.send(folder);
    });
    let folder = receiver.await.ok()??;
    folder.into_path().ok().map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
async fn start_run(
    app: AppHandle,
    state: State<'_, AppState>,
    project_dir: String,
    request: String,
    demo: bool,
) -> CommandResult<String> {
    let request = request.trim();
    if request.is_empty() {
        return Err("Describe what you want built.".to_string());
    }
    let project_dir = expand_home(project_dir.trim());
    if !demo && !project_dir.is_dir() {
        return Err(format!("Folder not found: {}", project_dir.display()));
    }

    let runner: Arc<dyn AgentRunner> =
        if demo { Arc::new(DemoRunner::new(DEMO_STEP_DELAY)) } else { state.claude_runner().await? };
    let orchestrator = Orchestrator::new(runner);

    // Every event of this run is forwarded to the window.
    let sink: EventSink = Arc::new(move |run_id: &str, event: &Event| {
        let _ = app.emit(RUN_EVENT, RunEvent { run_id, event });
    });
    let run = Run::new(project_dir, request, sink);

    let mut runs = state.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // One run at a time: stop whatever was still going.
    for previous in runs.values() {
        previous.run.stop();
    }
    runs.insert(run.id.clone(), ActiveRun { run: run.clone(), orchestrator: orchestrator.clone() });
    drop(runs);

    let run_id = run.id.clone();
    spawn_phase(run.clone(), async move { orchestrator.plan(&run).await });
    Ok(run_id)
}

/// Everything that has happened in a run so far, for a page that just (re)loaded.
#[tauri::command]
fn run_events(state: State<'_, AppState>, run_id: String) -> CommandResult<Vec<Event>> {
    Ok(state.find(&run_id)?.0.events())
}

#[tauri::command]
fn approve_plan(state: State<'_, AppState>, run_id: String, tasks: Vec<Task>) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    if run.state() != RunState::AwaitingApproval || run.is_busy() {
        return Err("There is no plan waiting for approval.".to_string());
    }
    let tasks: Vec<Task> = tasks.into_iter().filter(|task| !task.title.trim().is_empty()).collect();
    if tasks.is_empty() {
        return Err("The plan needs at least one task.".to_string());
    }
    spawn_phase(run.clone(), async move { orchestrator.build_and_review(&run, tasks).await });
    Ok(())
}

/// Send a reviewer's findings back to worker `worker`.
#[tauri::command]
fn send_back(state: State<'_, AppState>, run_id: String, worker: usize) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    let at_rest = matches!(run.state(), RunState::Done | RunState::Stopped | RunState::Failed);
    if !at_rest || run.is_busy() {
        return Err("Wait for the run to finish first.".to_string());
    }
    if run.review(worker).is_none() {
        return Err("That worker has no review to act on.".to_string());
    }
    spawn_phase(run.clone(), async move { orchestrator.fix(&run, worker).await });
    Ok(())
}

#[tauri::command]
fn stop_run(state: State<'_, AppState>, run_id: String) -> CommandResult<()> {
    state.find(&run_id)?.0.stop();
    Ok(())
}

/// Do one phase in the background so the command can return right away.
fn spawn_phase<F>(run: Arc<Run>, work: F)
where
    F: std::future::Future<Output = Result<(), agent_console_core::AgentError>> + Send + 'static,
{
    tauri::async_runtime::spawn(supervised(run, work));
}

/// Turn a leading `~` into the home folder, as a shell would.
fn expand_home(path: &str) -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from);
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            app_info,
            pick_folder,
            start_run,
            run_events,
            approve_plan,
            send_back,
            stop_run
        ])
        .run(tauri::generate_context!())
        .expect("error while running Agent Console");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_to_the_home_folder() {
        let home = PathBuf::from(env::var_os("HOME").expect("HOME is set"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/projects/x"), home.join("projects/x"));
        assert_eq!(expand_home("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_home("~other/x"), PathBuf::from("~other/x"));
    }
}
