//! The desktop app: a thin layer between the page in the window and the engine
//! in `crates/core`.
//!
//! The page calls the `#[tauri::command]` functions below with `invoke(...)`,
//! and receives progress as `run-event` events. Each command checks that the
//! action is allowed right now (with the `can_*` functions from the engine),
//! then starts the work in the background and returns. Nothing in here knows how
//! agents work; that is all in the core crate.

mod store;

use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_console_core::claude::{locate, ClaudeRunner};
use agent_console_core::config::{self, AuthMode, Role, Task, PLANNER_ID};
use agent_console_core::event::EventKind;
use agent_console_core::orchestrator::{
    can_approve, can_chat, can_finish_round, can_message, can_send_back, supervised, Isolation,
};
use agent_console_core::project;
use agent_console_core::run::EventSink;
use agent_console_core::settings::{self, Preset, Settings, SharedSettings};
use agent_console_core::worktrees::{self, FileChange, Workspace};
use agent_console_core::{AgentError, AgentRunner, Event, Orchestrator, Run};
use serde::{Deserialize, Serialize};
use store::{RunModels, RunStore, RunSummary};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{oneshot, OnceCell};

const RUN_EVENT: &str = "run-event";

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
    /// The settings in use right now, shared with every orchestrator. They are the
    /// chosen project's own set if it has one, else the general set.
    settings: SharedSettings,
    /// Every set of settings on disk: the general one and any project's own.
    saved_settings: Mutex<settings::Store>,
    /// Every run, saved as it happens.
    runs_on_disk: Arc<RunStore>,
}

impl AppState {
    fn new(saved_settings: settings::Store, runs_on_disk: Arc<RunStore>) -> Self {
        AppState {
            runs: Mutex::new(HashMap::new()),
            claude: OnceCell::new(),
            settings: settings::shared(saved_settings.for_project(None)),
            saved_settings: Mutex::new(saved_settings),
            runs_on_disk,
        }
    }

    fn saved_settings(&self) -> std::sync::MutexGuard<'_, settings::Store> {
        self.saved_settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Use these settings from now on, for every agent that starts next.
    fn apply_settings(&self, settings: &Settings) {
        *self.settings.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = settings.clone();
    }

    fn runs(&self) -> std::sync::MutexGuard<'_, HashMap<String, ActiveRun>> {
        self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn find(&self, run_id: &str) -> CommandResult<(Arc<Run>, Orchestrator)> {
        let runs = self.runs();
        let active = runs.get(run_id).ok_or("That run no longer exists. Start a new one.")?;
        Ok((active.run.clone(), active.orchestrator.clone()))
    }

    /// Find a run that can still be worked on. A run read back from disk cannot:
    /// its agents' sessions and its worktrees went with the app that made them.
    fn find_live(&self, run_id: &str) -> CommandResult<(Arc<Run>, Orchestrator)> {
        let found = self.find(run_id)?;
        if found.0.restored {
            return Err("This run was loaded from an earlier launch, so it can be read but not \
                        continued. Start a new run to carry on."
                .to_string());
        }
        Ok(found)
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
    /// What an empty field in the Settings window falls back to, and the allowed ranges.
    limits: Limits,
}

/// Shown in the Settings window as the placeholder of each turn-limit field and as
/// the ranges the fields accept.
#[derive(Serialize)]
struct Limits {
    dispatcher_turns: u32,
    planner_turns: u32,
    worker_turns: u32,
    reviewer_turns: u32,
    chat_turns: u32,
    max_turns: u32,
    max_workers: usize,
    max_notes: usize,
}

/// What opening a saved run tells the window.
#[derive(Serialize)]
struct Opened {
    /// False when the run can only be read.
    resumable: bool,
    /// Why not, in a few words, when it cannot.
    why: Option<String>,
}

impl Opened {
    fn of(run: &Run, why: Option<&str>) -> Opened {
        let resumable = run.resumable();
        Opened { resumable, why: if resumable { None } else { why.map(str::to_string) } }
    }
}

/// What the Changes window shows.
#[derive(Serialize)]
struct RoundChanges {
    files: Vec<FileChange>,
    /// The commit message offered in the window, from what the plan set out to do.
    suggested_message: String,
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
        limits: Limits {
            dispatcher_turns: config::DISPATCHER_MAX_TURNS,
            planner_turns: config::PLANNER_MAX_TURNS,
            worker_turns: config::WORKER_MAX_TURNS,
            reviewer_turns: config::REVIEWER_MAX_TURNS,
            chat_turns: config::CHAT_MAX_TURNS,
            max_turns: settings::MAX_TURNS_LIMIT,
            max_workers: config::WORKER_COUNT,
            max_notes: settings::MAX_NOTES_LENGTH,
        },
    }
}

/// What this project looks like it needs its agents to be able to run. Offered in
/// the Settings window; nothing is allowed until the user saves it.
#[tauri::command]
fn project_suggestions(project_dir: String) -> project::Suggestions {
    project::suggestions(&expand_home(project_dir.trim()))
}

/// Switch to a project: from now on its own settings are used, if it has a set.
#[tauri::command]
fn use_project(state: State<'_, AppState>, project_dir: String) -> Settings {
    let settings = state.saved_settings().for_project(Some(project_dir.trim()));
    state.apply_settings(&settings);
    settings
}

/// Check, save and apply new settings, either for one project or for all of them.
/// Returns them tidied (model names trimmed, empty command lines dropped).
#[tauri::command]
fn save_settings(
    state: State<'_, AppState>,
    settings: Settings,
    project_dir: Option<String>,
    own_settings: bool,
) -> CommandResult<Settings> {
    let project_dir = project_dir.map(|dir| dir.trim().to_string()).filter(|dir| !dir.is_empty());
    let saved = state.saved_settings().save(project_dir.as_deref(), settings, own_settings)?;
    state.apply_settings(&saved);
    Ok(saved)
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

    // Every event of this run goes to the window and into the run's own file.
    let sink = window_sink(app, Some(state.runs_on_disk.clone()));
    let run = Run::new(project_dir, message.trim(), sink);
    can_message(&run, &message)?;
    let settings = state.settings.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    state.runs_on_disk.start(&run, RunModels::of(&settings));

    let mut runs = state.runs();
    // One run at a time: stop whatever was still going.
    for previous in runs.values() {
        previous.run.stop();
    }
    runs.insert(run.id.clone(), ActiveRun { run: run.clone(), orchestrator: orchestrator.clone() });
    drop(runs);

    dispatch(state.runs_on_disk.clone(), run.clone(), orchestrator, message, mode);
    Ok(run.id.clone())
}

/// Send a message from the main message box to an existing run.
#[tauri::command]
fn send(state: State<'_, AppState>, run_id: String, message: String, mode: SendMode) -> CommandResult<()> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_message(&run, &message)?;
    dispatch(state.runs_on_disk.clone(), run, orchestrator, message, mode);
    Ok(())
}

fn dispatch(
    store: Arc<RunStore>,
    run: Arc<Run>,
    orchestrator: Orchestrator,
    message: String,
    mode: SendMode,
) {
    let message = message.trim().to_string();
    let task_run = run.clone();
    spawn_phase(store, run, async move {
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
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_chat(&run, &agent, &message)?;
    let message = message.trim().to_string();
    let task_run = run.clone();
    spawn_phase(state.runs_on_disk.clone(), run, async move {
        orchestrator.chat(&task_run, &agent, &message).await
    });
    Ok(())
}

/// Ask the planner for a plan from the conversation so far, with optional extra text.
#[tauri::command]
fn make_plan(state: State<'_, AppState>, run_id: String, guidance: Option<String>) -> CommandResult<()> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    if !run.is_at_rest() {
        return Err("Wait until the agents have finished.".to_string());
    }
    let task_run = run.clone();
    spawn_phase(state.runs_on_disk.clone(), run, async move {
        orchestrator.plan(&task_run, guidance.as_deref()).await
    });
    Ok(())
}

#[tauri::command]
fn approve_plan(state: State<'_, AppState>, run_id: String, tasks: Vec<Task>) -> CommandResult<()> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_approve(&run)?;
    let tasks: Vec<Task> = tasks.into_iter().filter(|task| !task.title.trim().is_empty()).collect();
    if tasks.is_empty() {
        return Err("The plan needs at least one task.".to_string());
    }
    let task_run = run.clone();
    spawn_phase(state.runs_on_disk.clone(), run, async move { orchestrator.build(&task_run, tasks).await });
    Ok(())
}

/// Give worker `worker` the reviewer's findings for its task.
#[tauri::command]
fn send_back(state: State<'_, AppState>, run_id: String, worker: usize) -> CommandResult<()> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_send_back(&run, worker)?;
    let task_run = run.clone();
    spawn_phase(
        state.runs_on_disk.clone(),
        run,
        async move { orchestrator.send_back(&task_run, worker).await },
    );
    Ok(())
}

#[tauri::command]
fn review_again(state: State<'_, AppState>, run_id: String) -> CommandResult<()> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_finish_round(&run)?;
    let task_run = run.clone();
    spawn_phase(state.runs_on_disk.clone(), run, async move { orchestrator.review(&task_run).await });
    Ok(())
}

/// Every file the open round would change, for the Changes window.
#[tauri::command]
async fn round_changes(state: State<'_, AppState>, run_id: String) -> CommandResult<RoundChanges> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_finish_round(&run)?;
    let files = orchestrator.round_changes(&run).await?;
    // The reviewer has read every change together, so its message beats anything
    // worked out from the plan. The plan, and then the original request, are what
    // is left when there is no review yet.
    let reviewed = run.review().map(|review| review.commit_message).unwrap_or_default();
    let summary = run.plan_summary();
    let suggested_message = [reviewed, summary, run.request.clone()]
        .into_iter()
        .find(|text| !text.trim().is_empty())
        .unwrap_or_default();
    Ok(RoundChanges { files, suggested_message: first_line(&suggested_message) })
}

/// Apply the round's changes. With a message, they are committed too; without
/// one, they are left staged for you to commit yourself. `push` sends the commit
/// to the remote afterwards, and is ignored without a message: there would be
/// nothing of this round to push.
#[tauri::command]
fn apply_changes(
    state: State<'_, AppState>,
    run_id: String,
    commit_message: Option<String>,
    push: Option<bool>,
) -> CommandResult<()> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_finish_round(&run)?;
    let message = commit_message.map(|text| text.trim().to_string()).filter(|text| !text.is_empty());
    let push = push.unwrap_or(false) && message.is_some();
    let task_run = run.clone();
    spawn_phase(state.runs_on_disk.clone(), run, async move {
        orchestrator.apply(&task_run, message.as_deref(), push).await
    });
    Ok(())
}

/// Commit whatever is uncommitted in the project folder, as the user.
///
/// What the window would otherwise have told them to go and do in a terminal. Only
/// ever called from the button on the "uncommitted changes" error, so the message is
/// the one that error offered.
#[tauri::command]
async fn tidy_project(
    state: State<'_, AppState>,
    project_dir: String,
    message: String,
) -> CommandResult<usize> {
    let message = message.trim();
    if message.is_empty() {
        return Err("A commit needs a message.".to_string());
    }
    let _ = &state; // the project folder is all this needs
    worktrees::commit_project(Path::new(&project_dir), message).await
}

/// Every run this app has saved, newest first.
///
/// This reads every saved run's file, so it is done on a thread of its own: a
/// synchronous command runs on the thread that draws the window, and a long
/// history would freeze it while the Runs window opened.
#[tauri::command]
async fn recent_runs(state: State<'_, AppState>) -> CommandResult<Vec<RunSummary>> {
    let store = state.runs_on_disk.clone();
    tokio::task::spawn_blocking(move || store.list())
        .await
        .map_err(|_| "The saved runs could not be read.".to_string())
}

/// Load a saved run so the window can show it again, and carry on with it when
/// everything it needs is still there.
#[tauri::command]
async fn open_run(app: AppHandle, state: State<'_, AppState>, run_id: String) -> CommandResult<Opened> {
    if let Some(run) = state.runs().get(&run_id).map(|active| active.run.clone()) {
        return Ok(Opened::of(&run, None));
    }
    let store = state.runs_on_disk.clone();
    let id = run_id.clone();
    // Reading a long run's file is slow enough to be worth keeping off the
    // thread that draws the window.
    let (head, events) = tokio::task::spawn_blocking(move || store.read(&id))
        .await
        .map_err(|_| "That run could not be read.".to_string())??;
    let runner: Arc<dyn AgentRunner> = state.claude_runner().await?;
    let orchestrator = Orchestrator::new(runner, state.settings.clone(), Isolation::Worktrees);

    // Three things have to be there to carry on: what was saved about the run,
    // its project, and, if a round was open, its worktrees.
    let snapshot = state.runs_on_disk.read_state(&run_id);
    let project_dir = PathBuf::from(&head.project_dir);
    let mut why = None;
    let mut snapshot = match (&snapshot, project_dir.is_dir()) {
        (None, _) => {
            why = Some("it was saved by a version of the app that did not keep enough to carry on");
            None
        }
        (_, false) => {
            why = Some("its project folder is no longer where it was");
            None
        }
        (Some(_), true) => snapshot,
    };

    let mut workspace = None;
    if let Some(saved) = &snapshot {
        if let Some(round) = &saved.round {
            if let Some(kept) = &round.workspace {
                let opened =
                    Workspace::reopen(&project_dir, &head.id, saved.rounds_started, kept.workers, &kept.base)
                        .await;
                match opened {
                    Ok(reopened) => workspace = Some(Arc::new(reopened)),
                    Err(problem) => {
                        why = Some("the copies of the project its workers used are gone");
                        let _ = problem;
                        snapshot = None;
                    }
                }
            }
        }
    }

    let run = Run::from_saved(
        head.id,
        &head.project_dir,
        head.request,
        events,
        snapshot,
        workspace,
        window_sink(app, Some(state.runs_on_disk.clone())),
    );
    let opened = Opened::of(&run, why);
    state.runs().insert(run.id.clone(), ActiveRun { run, orchestrator });
    Ok(opened)
}

#[tauri::command]
fn discard_changes(state: State<'_, AppState>, run_id: String) -> CommandResult<()> {
    let (run, orchestrator) = state.find_live(&run_id)?;
    can_finish_round(&run)?;
    let task_run = run.clone();
    spawn_phase(state.runs_on_disk.clone(), run, async move { orchestrator.discard(&task_run).await });
    Ok(())
}

/// Forget one agent's session, so its next message starts fresh. The way out of
/// a conversation that has grown long enough to be expensive on every turn.
#[tauri::command]
fn fresh_session(state: State<'_, AppState>, run_id: String, agent: String) -> CommandResult<()> {
    let (run, _) = state.find_live(&run_id)?;
    if !run.forget_session(&agent) {
        return Err("That agent has no conversation to forget.".to_string());
    }
    let text = "Starting fresh here: this agent will not remember what was said before. \
                The rest of the team is unchanged."
        .to_string();
    run.emit(Some(&agent), EventKind::Notice { text });
    state.runs_on_disk.save_state(&run);
    Ok(())
}

#[tauri::command]
fn stop_run(state: State<'_, AppState>, run_id: String) -> CommandResult<()> {
    state.find_live(&run_id)?.0.stop();
    Ok(())
}

/// Keep only the first line, and not too much of it: a commit message's first
/// line is its subject.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().chars().take(72).collect::<String>().trim().to_string()
}

/// Forward every event of a run to the window, and save it when there is a store.
fn window_sink(app: AppHandle, store: Option<Arc<RunStore>>) -> EventSink {
    Arc::new(move |run_id: &str, event: &Event| {
        if let Some(store) = &store {
            store.append(run_id, event);
        }
        let _ = app.emit(RUN_EVENT, RunEvent { run_id, event });
    })
}

/// Do one phase in the background so the command can return right away, then
/// save where the run got to, so it can be picked up again after a restart.
///
/// The phase runs in a task of its own so that a panic inside it is caught here.
/// Without that, the run would be left waiting for a phase that had already
/// died, with Stop unable to help, and the only way out would be to restart.
fn spawn_phase<F>(store: Arc<RunStore>, run: Arc<Run>, work: F)
where
    F: Future<Output = Result<(), AgentError>> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let phase = tauri::async_runtime::spawn(supervised(run.clone(), work));
        if phase.await.is_err() {
            run.phase_crashed(
                "Something went wrong inside the app and that step stopped. Nothing was lost: \
                 any open round is still there. If it happens again, it is worth reporting.",
            );
        }
        store.save_state(&run);
    });
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
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // ~/Library/Application Support/<identifier> on macOS: settings.json,
            // projects.json, and one file per run under runs/.
            let folder = app.path().app_config_dir()?;
            let (saved_settings, problems) = settings::Store::load(&folder);
            for problem in problems {
                eprintln!("{problem}");
            }
            app.manage(AppState::new(saved_settings, Arc::new(RunStore::new(&folder))));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            use_project,
            project_suggestions,
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
            round_changes,
            apply_changes,
            discard_changes,
            stop_run,
            fresh_session,
            recent_runs,
            tidy_project,
            open_run
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
