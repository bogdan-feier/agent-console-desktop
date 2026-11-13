//! The desktop app: a thin layer between the page in the window and the engine
//! in `crates/core`.
//!
//! The page calls the `#[tauri::command]` functions below with `invoke(...)`,
//! and receives progress as `run-event` events. Each command checks that the
//! action is allowed right now (with the `can_*` functions from the engine),
//! then starts the work in the background and returns. Nothing in here knows how
//! agents work; that is all in the core crate.

use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_console_core::claude::{locate, ClaudeRunner};
use agent_console_core::config::{self, AuthMode, Role, Task, PLANNER_ID};
use agent_console_core::orchestrator::{
    can_approve, can_chat, can_finish_round, can_message, can_send_back, supervised, Isolation,
};
use agent_console_core::run::EventSink;
use agent_console_core::settings::{self, Preset, Settings, SharedSettings};
use agent_console_core::{AgentError, AgentRunner, Event, Orchestrator, Run};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{oneshot, OnceCell};

const RUN_EVENT: &str = "run-event";
const SETTINGS_FILE: &str = "settings.json";

/// Commands return `Err(String)` for anything the user should read.
type CommandResult<T> = Result<T, String>;

/// One run plus the orchestrator that drives it.
struct ActiveRun {
    run: Arc<Run>,
    orchestrator: Orchestrator,
}

/// Shared by every command. Tauri hands it out as `State<AppState>`.
struct AppState {
    runs: Mutex<HashMap<String, ActiveRun>>,
    /// Finding `claude` starts a shell, so it is done once, on first use.
    claude: OnceCell<Result<Arc<ClaudeRunner>, String>>,
    /// The current settings, shared with every orchestrator.
    settings: SharedSettings,
    /// Where the settings are saved: the app's folder in ~/Library/Application Support.
    settings_path: PathBuf,
}

impl AppState {
    fn new(settings: Settings, settings_path: PathBuf) -> Self {
        AppState {
            runs: Mutex::new(HashMap::new()),
            claude: OnceCell::new(),
            settings: settings::shared(settings),
            settings_path,
        }
    }

    fn runs(&self) -> std::sync::MutexGuard<'_, HashMap<String, ActiveRun>> {
        self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn find(&self, run_id: &str) -> CommandResult<(Arc<Run>, Orchestrator)> {
        let runs = self.runs();
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
}

#[derive(Serialize)]
struct PresetInfo {
    preset: Preset,
    settings: Settings,
}

#[derive(Serialize)]
struct AppInfo {
    agents: Vec<AgentInfo>,
    auth: AuthMode,
    settings: Settings,
    /// The built-in defaults, for the "Reset to defaults" button.
    default_settings: Settings,
    /// The ready-made combinations offered in the Settings window.
    presets: Vec<PresetInfo>,
}

/// What the page receives for every event: which run it belongs to, and the event.
#[derive(Clone, Serialize)]
struct RunEvent<'a> {
    run_id: &'a str,
    event: &'a Event,
}

/// Where a message from the main message box goes.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SendMode {
    /// To the dispatcher, which answers it or passes it on.
    Dispatcher,
    /// Straight to the planner, to talk it through.
    Brainstorm,
    /// Straight to the planner, to make a plan.
    Plan,
}

#[tauri::command]
fn app_info(state: State<'_, AppState>) -> AppInfo {
    let current = settings::current(&state.settings);
    let agents = config::all_agents(&current)
        .into_iter()
        .map(|spec| AgentInfo { id: spec.id, role: spec.role, label: spec.label })
        .collect();
    AppInfo {
        agents,
        auth: config::AUTH,
        settings: current,
        default_settings: Settings::default(),
        presets: Preset::ALL
            .into_iter()
            .map(|preset| PresetInfo { preset, settings: Settings::preset(preset) })
            .collect(),
    }
}

/// Check, save and apply new settings. Returns them tidied (model names trimmed).
#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: Settings) -> CommandResult<Settings> {
    let settings = settings.validated()?;
    settings::save(&state.settings_path, &settings)?;
    *state.settings.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = settings.clone();
    Ok(settings)
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

/// Start a new run on a project with its first message.
#[tauri::command]
async fn start_run(
    app: AppHandle,
    state: State<'_, AppState>,
    project_dir: String,
    message: String,
    mode: SendMode,
) -> CommandResult<String> {
    let project_dir = expand_home(project_dir.trim());
    if !project_dir.is_dir() {
        return Err(format!("Folder not found: {}", project_dir.display()));
    }
    // Never lose work silently: a round whose changes were not applied or discarded
    // keeps its run alive.
    if state.runs().values().any(|active| active.run.has_open_round()) {
        return Err("The current run has changes waiting. Apply or discard them first.".to_string());
    }

    let runner: Arc<dyn AgentRunner> = state.claude_runner().await?;
    let orchestrator = Orchestrator::new(runner, state.settings.clone(), Isolation::Worktrees);

    // Every event of this run is forwarded to the window.
    let sink: EventSink = Arc::new(move |run_id: &str, event: &Event| {
        let _ = app.emit(RUN_EVENT, RunEvent { run_id, event });
    });
    let run = Run::new(project_dir, message.trim(), sink);
    can_message(&run, &message)?;

    let mut runs = state.runs();
    // One run at a time: stop whatever was still going.
    for previous in runs.values() {
        previous.run.stop();
    }
    runs.insert(run.id.clone(), ActiveRun { run: run.clone(), orchestrator: orchestrator.clone() });
    drop(runs);

    dispatch(run.clone(), orchestrator, message, mode);
    Ok(run.id.clone())
}

/// Send a message from the main message box to an existing run.
#[tauri::command]
fn send(state: State<'_, AppState>, run_id: String, message: String, mode: SendMode) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    can_message(&run, &message)?;
    dispatch(run, orchestrator, message, mode);
    Ok(())
}

fn dispatch(run: Arc<Run>, orchestrator: Orchestrator, message: String, mode: SendMode) {
    let message = message.trim().to_string();
    let task_run = run.clone();
    spawn_phase(run, async move {
        match mode {
            SendMode::Dispatcher => orchestrator.ask(&task_run, &message).await,
            SendMode::Brainstorm => orchestrator.chat(&task_run, PLANNER_ID, &message).await,
            SendMode::Plan => orchestrator.plan(&task_run, Some(&message)).await,
        }
    });
}

/// Everything that has happened in a run so far, for a page that just (re)loaded.
#[tauri::command]
fn run_events(state: State<'_, AppState>, run_id: String) -> CommandResult<Vec<Event>> {
    Ok(state.find(&run_id)?.0.events())
}

/// Send the user's message directly to one agent (from the box in its panel).
#[tauri::command]
fn send_message(
    state: State<'_, AppState>,
    run_id: String,
    agent: String,
    message: String,
) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    can_chat(&run, &agent, &message)?;
    let message = message.trim().to_string();
    let task_run = run.clone();
    spawn_phase(run, async move { orchestrator.chat(&task_run, &agent, &message).await });
    Ok(())
}

/// Ask the planner for a plan from the conversation so far, with optional extra text.
#[tauri::command]
fn make_plan(state: State<'_, AppState>, run_id: String, guidance: Option<String>) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    if !run.is_at_rest() {
        return Err("Wait until the agents have finished.".to_string());
    }
    let task_run = run.clone();
    spawn_phase(run, async move { orchestrator.plan(&task_run, guidance.as_deref()).await });
    Ok(())
}

#[tauri::command]
fn approve_plan(state: State<'_, AppState>, run_id: String, tasks: Vec<Task>) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    can_approve(&run)?;
    let tasks: Vec<Task> = tasks.into_iter().filter(|task| !task.title.trim().is_empty()).collect();
    if tasks.is_empty() {
        return Err("The plan needs at least one task.".to_string());
    }
    let task_run = run.clone();
    spawn_phase(run, async move { orchestrator.build(&task_run, tasks).await });
    Ok(())
}

/// Give worker `worker` the reviewer's findings for its task.
#[tauri::command]
fn send_back(state: State<'_, AppState>, run_id: String, worker: usize) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    can_send_back(&run, worker)?;
    let task_run = run.clone();
    spawn_phase(run, async move { orchestrator.send_back(&task_run, worker).await });
    Ok(())
}

#[tauri::command]
fn review_again(state: State<'_, AppState>, run_id: String) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    can_finish_round(&run)?;
    let task_run = run.clone();
    spawn_phase(run, async move { orchestrator.review(&task_run).await });
    Ok(())
}

#[tauri::command]
fn apply_changes(state: State<'_, AppState>, run_id: String) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    can_finish_round(&run)?;
    let task_run = run.clone();
    spawn_phase(run, async move { orchestrator.apply(&task_run).await });
    Ok(())
}

#[tauri::command]
fn discard_changes(state: State<'_, AppState>, run_id: String) -> CommandResult<()> {
    let (run, orchestrator) = state.find(&run_id)?;
    can_finish_round(&run)?;
    let task_run = run.clone();
    spawn_phase(run, async move { orchestrator.discard(&task_run).await });
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
    F: Future<Output = Result<(), AgentError>> + Send + 'static,
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
        .setup(|app| {
            // ~/Library/Application Support/<identifier>/settings.json on macOS.
            let settings_path = app.path().app_config_dir()?.join(SETTINGS_FILE);
            let (settings, problem) = settings::load(&settings_path);
            if let Some(problem) = problem {
                eprintln!("{problem}");
            }
            app.manage(AppState::new(settings, settings_path));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            save_settings,
            pick_folder,
            start_run,
            send,
            run_events,
            send_message,
            make_plan,
            approve_plan,
            send_back,
            review_again,
            apply_changes,
            discard_changes,
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

    #[test]
    fn send_modes_come_from_the_page_in_snake_case() {
        let mode: SendMode = serde_json::from_str("\"brainstorm\"").unwrap();
        assert!(matches!(mode, SendMode::Brainstorm));
        assert!(matches!(serde_json::from_str::<SendMode>("\"dispatcher\"").unwrap(), SendMode::Dispatcher));
        assert!(serde_json::from_str::<SendMode>("\"build\"").is_err());
    }
}
