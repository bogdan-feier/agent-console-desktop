//! Settings you change in the app's Settings window: the model, effort and turn limit
//! for each role, the spending cap, what agents may do (shell commands, web search),
//! how many workers a plan may use, and notes added to every agent's instructions.
//! They are saved as JSON so they survive a restart.
//!
//! The defaults come from the constants in `config.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::config::{
    CHECK_COMMANDS, DISPATCHER_MODEL, MAX_BUDGET_USD_PER_AGENT, PLANNER_MODEL, REVIEWER_MODEL, WORKER_COUNT,
    WORKER_MODEL,
};

/// Highest spending cap the Settings window accepts, in US dollars per agent session.
pub const MAX_BUDGET_LIMIT_USD: f64 = 1000.0;
/// Highest turn limit the Settings window accepts for one session.
pub const MAX_TURNS_LIMIT: u32 = 500;
/// Longest "notes for every agent" text, in characters. It is sent with every turn
/// of every agent (mostly from the cache), so it should stay short.
pub const MAX_NOTES_LENGTH: usize = 4000;
/// Longest check command, in characters.
pub const MAX_CHECK_COMMAND_LENGTH: usize = 300;
const MAX_MODEL_NAME_LENGTH: usize = 100;
const MAX_COMMAND_LENGTH: usize = 200;
const MAX_COMMANDS: usize = 50;

/// How hard the model thinks before answering. Passed to `claude --effort`.
/// Higher is slower and uses more of your limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_flag(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// Model, effort and turn limit for one role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleSettings {
    /// "opus", "sonnet", "haiku", or a full model ID.
    pub model: String,
    /// `None` means Claude Code's own default for that model.
    #[serde(default)]
    pub effort: Option<Effort>,
    /// Most turns one session of this role may take. `None` means the role's default
    /// (the `*_MAX_TURNS` constants in `config.rs`).
    #[serde(default)]
    pub max_turns: Option<u32>,
}

impl RoleSettings {
    fn new(model: &str, effort: Option<Effort>) -> Self {
        RoleSettings { model: model.to_string(), effort, max_turns: None }
    }
}

/// Ready-made combinations for the Settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// Least usage: Sonnet plans, low effort for the workers.
    Lean,
    /// The default: Opus plans, Sonnet builds and reviews, moderate effort.
    Balanced,
    /// Most careful: high effort, Opus also reviews. Uses the most.
    Thorough,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::Lean, Preset::Balanced, Preset::Thorough];
}

/// `#[serde(default)]`: a field missing from the file (say, after an update adds
/// one) takes its value from `Default` instead of making the whole file unreadable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `alias`: settings saved while this role was still called "jarvis" load too.
    #[serde(alias = "jarvis")]
    pub dispatcher: RoleSettings,
    pub planner: RoleSettings,
    pub worker: RoleSettings,
    pub reviewer: RoleSettings,
    /// Spending cap per agent session in US dollars; `None` means no cap.
    pub max_budget_usd: Option<f64>,
    /// Give agents the MCP servers configured in Claude Code. Off by default: every
    /// server adds its tool descriptions to every turn of every agent, and the agents
    /// here only need file and shell tools.
    pub use_mcp_servers: bool,

    // --- Permissions ---
    /// Shell commands workers and the reviewer may run, written as you would type them
    /// (`cargo test`, `npm run lint *`). A `*` matches the rest of the command. Anything
    /// not on the list is refused, because nobody is there to approve it. An empty list
    /// takes the shell away from them completely.
    pub allowed_commands: Vec<String>,
    /// Let the dispatcher and the planner use web search and fetch web pages. Only these
    /// two: they cannot edit files or run commands, so a web page that tries to give
    /// them instructions cannot make them change anything.
    pub web_access: bool,

    // --- Team and limits ---
    /// Most workers (and so tasks) one plan may use, from 1 to `WORKER_COUNT`.
    pub worker_count: usize,
    /// Turn limit when you message an agent directly. `None` means `CHAT_MAX_TURNS`.
    pub chat_max_turns: Option<u32>,

    // --- Instructions ---
    /// Added to every agent's instructions, for rules about this project or how you
    /// like to work ("use uv, never pip"). Empty adds nothing.
    pub notes: String,

    // --- Merging and checking ---
    /// When two workers changed the same lines, let an agent merge their work instead
    /// of giving up on the round. It only touches the clashing files, and the reviewer
    /// then checks the result like any other change.
    pub resolve_conflicts: bool,
    /// A command the app itself runs on the merged changes before the reviewer sees
    /// them, such as `cargo test`. Its output goes into the reviewer's prompt, which
    /// costs no turns. Empty means no check.
    pub check_command: String,

    /// True when these are one project's own settings rather than the general ones.
    /// Set when they are handed to the window; never read back from it or saved.
    #[serde(skip_deserializing)]
    pub own: bool,
}

/// The default list of allowed commands: `CHECK_COMMANDS` without the `Bash(...)` around them.
pub fn default_commands() -> Vec<String> {
    CHECK_COMMANDS
        .iter()
        .map(|rule| rule.trim_start_matches("Bash(").trim_end_matches(')').to_string())
        .collect()
}

impl Default for Settings {
    fn default() -> Self {
        Settings::preset(Preset::Balanced)
    }
}

impl Settings {
    pub fn preset(preset: Preset) -> Settings {
        use Effort::{High, Low, Medium};
        // The dispatcher stays on Haiku in every preset: routing and quick answers do not need
        // more, and it runs on every message. Not every Haiku version takes an effort
        // level, so it is left unset.
        let dispatcher = RoleSettings::new(DISPATCHER_MODEL, None);
        let (planner, worker, reviewer, budget) = match preset {
            Preset::Lean => (
                RoleSettings::new("sonnet", Some(Medium)),
                RoleSettings::new("sonnet", Some(Low)),
                // One reviewer checks every task together, so it stays on Sonnet.
                RoleSettings::new("sonnet", Some(Low)),
                Some(3.0),
            ),
            Preset::Balanced => (
                RoleSettings::new(PLANNER_MODEL, Some(Medium)),
                RoleSettings::new(WORKER_MODEL, Some(Medium)),
                RoleSettings::new(REVIEWER_MODEL, Some(Medium)),
                MAX_BUDGET_USD_PER_AGENT,
            ),
            Preset::Thorough => (
                RoleSettings::new("opus", Some(High)),
                RoleSettings::new("sonnet", Some(High)),
                RoleSettings::new("opus", Some(Medium)),
                Some(10.0),
            ),
        };
        Settings {
            dispatcher,
            planner,
            worker,
            reviewer,
            max_budget_usd: budget,
            use_mcp_servers: false,
            allowed_commands: default_commands(),
            web_access: false,
            worker_count: WORKER_COUNT,
            chat_max_turns: None,
            notes: String::new(),
            resolve_conflicts: true,
            check_command: String::new(),
            own: false,
        }
    }
}

/// Settings shared between the app and every running orchestrator. A change is
/// picked up by the next agent that starts. `RwLock`: many readers or one writer.
pub type SharedSettings = Arc<RwLock<Settings>>;

pub fn shared(settings: Settings) -> SharedSettings {
    Arc::new(RwLock::new(settings))
}

/// A copy of the current settings.
pub fn current(shared: &SharedSettings) -> Settings {
    shared.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

impl Settings {
    /// Check the settings and tidy them (trim names and commands, drop empty lines).
    /// Errors are shown to the user.
    pub fn validated(mut self) -> Result<Settings, String> {
        for (label, role) in [
            ("Dispatcher", &mut self.dispatcher),
            ("Planner", &mut self.planner),
            ("Workers", &mut self.worker),
            ("Reviewer", &mut self.reviewer),
        ] {
            role.model = role.model.trim().to_string();
            check_model(&role.model).map_err(|problem| format!("{label}: {problem}"))?;
            check_turns(role.max_turns).map_err(|problem| format!("{label}: {problem}"))?;
        }
        check_turns(self.chat_max_turns).map_err(|problem| format!("Direct messages: {problem}"))?;

        if !(1..=WORKER_COUNT).contains(&self.worker_count) {
            return Err(format!("The number of workers must be from 1 to {WORKER_COUNT}."));
        }

        let mut commands: Vec<String> = Vec::new();
        for command in &self.allowed_commands {
            let command = command.trim().to_string();
            if command.is_empty() || commands.contains(&command) {
                continue;
            }
            check_command(&command)?;
            commands.push(command);
        }
        if commands.len() > MAX_COMMANDS {
            return Err(format!("Allow at most {MAX_COMMANDS} commands."));
        }
        self.allowed_commands = commands;

        self.check_command = self.check_command.trim().to_string();
        if self.check_command.chars().count() > MAX_CHECK_COMMAND_LENGTH {
            return Err(format!("The check command can be at most {MAX_CHECK_COMMAND_LENGTH} characters."));
        }
        if self.check_command.contains(['\n', '\r']) {
            return Err("The check command must be a single line.".to_string());
        }

        self.notes = self.notes.trim().to_string();
        if self.notes.chars().count() > MAX_NOTES_LENGTH {
            return Err(format!(
                "Notes for every agent can be at most {MAX_NOTES_LENGTH} characters: they are \
                 sent with every turn of every agent."
            ));
        }

        if let Some(budget) = self.max_budget_usd {
            if !(budget > 0.0 && budget <= MAX_BUDGET_LIMIT_USD) {
                return Err(format!(
                    "The spending cap must be more than 0 and at most {MAX_BUDGET_LIMIT_USD} dollars, \
                     or empty for no cap."
                ));
            }
        }
        Ok(self)
    }
}

/// `None` (the role's default) or a number from 1 to `MAX_TURNS_LIMIT`.
fn check_turns(turns: Option<u32>) -> Result<(), String> {
    match turns {
        Some(turns) if !(1..=MAX_TURNS_LIMIT).contains(&turns) => {
            Err(format!("the turn limit must be from 1 to {MAX_TURNS_LIMIT}, or empty for the default."))
        }
        _ => Ok(()),
    }
}

/// An allowed command becomes a permission rule, `Bash(<command>)`, and all the rules
/// are passed to `claude` as one comma-separated list. So a command must not contain
/// a comma or brackets (they would break the list or the rule), nor a line break.
/// A lone `*` would allow every command, which is too much for agents that run
/// unattended.
fn check_command(command: &str) -> Result<(), String> {
    let problem = |why: &str| Err(format!("Allowed command \"{command}\": {why}"));
    if command.chars().count() > MAX_COMMAND_LENGTH {
        return problem("too long.");
    }
    if command.contains([',', '(', ')', '\n', '\r']) {
        return problem("commas and brackets cannot be used.");
    }
    if command.trim_matches(|c: char| c == '*' || c.is_whitespace()).is_empty() {
        return problem("this would allow every command. Name the program, for example \"npm test *\".");
    }
    Ok(())
}

/// Model names become a command-line argument, so only allow the characters
/// model names actually use. A leading dash could be read as a flag.
fn check_model(model: &str) -> Result<(), String> {
    if model.is_empty() {
        return Err("choose a model.".to_string());
    }
    if model.len() > MAX_MODEL_NAME_LENGTH {
        return Err("that model name is too long.".to_string());
    }
    if model.starts_with('-') {
        return Err("a model name cannot start with a dash.".to_string());
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || "-._:[]".contains(c);
    if !model.chars().all(allowed) {
        return Err(format!("\"{model}\" is not a valid model name."));
    }
    Ok(())
}

/// Read settings from `path`. A missing file gives the defaults. A broken file
/// also gives the defaults, plus a message saying so.
pub fn load(path: &Path) -> (Settings, Option<String>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return (Settings::default(), None),
    };
    match serde_json::from_str::<Settings>(&text).map_err(|e| e.to_string()).and_then(Settings::validated) {
        Ok(settings) => (settings, None),
        Err(problem) => (
            Settings::default(),
            Some(format!("Ignored the settings in {} ({problem}); using the defaults.", path.display())),
        ),
    }
}

/// Write settings to `path`, creating its folder if needed.
///
/// Writes to a temporary file first and then renames it, so a crash halfway
/// through can never leave a half-written settings file behind.
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let problem = |error: std::io::Error| format!("Could not save the settings: {error}");
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).map_err(problem)?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, json).map_err(problem)?;
    fs::rename(&temporary, path).map_err(problem)
}

/// Where the settings live: one general set, plus a set of its own for any project
/// that wants one.
///
/// The general set is `settings.json`, the same file older versions wrote. Projects
/// with their own settings are in `projects.json`, keyed by folder.
pub struct Store {
    general_path: PathBuf,
    projects_path: PathBuf,
    general: Settings,
    projects: BTreeMap<String, Settings>,
}

impl Store {
    /// Read both files from `folder`. Anything unreadable is replaced by the
    /// defaults, and said so in the returned messages.
    pub fn load(folder: &Path) -> (Store, Vec<String>) {
        let general_path = folder.join("settings.json");
        let projects_path = folder.join("projects.json");
        let (general, problem) = load(&general_path);
        let mut messages: Vec<String> = problem.into_iter().collect();
        let mut projects: BTreeMap<String, Settings> = match fs::read_to_string(&projects_path) {
            Err(_) => BTreeMap::new(),
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|problem| {
                messages.push(format!(
                    "Ignored the per-project settings in {} ({problem}); every project uses the general set.",
                    projects_path.display()
                ));
                BTreeMap::new()
            }),
        };
        // A project's set is checked like any other: a bad one is dropped, not kept.
        projects.retain(|_, settings| settings.clone().validated().is_ok());
        (Store { general_path, projects_path, general, projects }, messages)
    }

    /// The settings a project should use: its own if it has a set, else the general
    /// ones. `own` says which it was.
    pub fn for_project(&self, project_dir: Option<&str>) -> Settings {
        match project_dir.and_then(|dir| self.projects.get(dir)) {
            Some(settings) => Settings { own: true, ..settings.clone() },
            None => Settings { own: false, ..self.general.clone() },
        }
    }

    /// Save `settings`, either as this project's own set or as the general one.
    /// Turning `own` off for a project also removes the set it had.
    pub fn save(
        &mut self,
        project_dir: Option<&str>,
        settings: Settings,
        own: bool,
    ) -> Result<Settings, String> {
        let settings = Settings { own: false, ..settings.validated()? };
        match (own, project_dir) {
            (true, Some(dir)) => {
                self.projects.insert(dir.to_string(), settings.clone());
                self.write_projects()?;
            }
            (false, dir) => {
                if dir.is_some_and(|dir| self.projects.remove(dir).is_some()) {
                    self.write_projects()?;
                }
                self.general = settings.clone();
                save(&self.general_path, &settings)?;
            }
            // "Own settings" with no project chosen: nothing to attach them to.
            (true, None) => return Err("Choose a project before giving it settings of its own.".to_string()),
        }
        Ok(Settings { own: own && project_dir.is_some(), ..settings })
    }

    fn write_projects(&self) -> Result<(), String> {
        let problem = |error: std::io::Error| format!("Could not save the settings: {error}");
        if let Some(folder) = self.projects_path.parent() {
            fs::create_dir_all(folder).map_err(problem)?;
        }
        let json = serde_json::to_string_pretty(&self.projects).map_err(|error| error.to_string())?;
        let temporary = self.projects_path.with_extension("json.tmp");
        fs::write(&temporary, json).map_err(problem)?;
        fs::rename(&temporary, &self.projects_path).map_err(problem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_can_have_settings_of_its_own() {
        let folder = tempfile::tempdir().unwrap();
        let (mut store, messages) = Store::load(folder.path());
        assert!(messages.is_empty());
        assert!(
            !store.for_project(Some("/code/app")).own,
            "without its own set, a project gets the general one"
        );

        let lean = Settings { notes: "project rule".into(), ..Settings::preset(Preset::Lean) };
        let saved = store.save(Some("/code/app"), lean, true).unwrap();
        assert!(saved.own);
        assert_eq!(store.for_project(Some("/code/app")).notes, "project rule");
        assert_eq!(store.for_project(Some("/code/other")).notes, "", "other projects are untouched");

        // Reading the files again gives the same answer.
        let (store, _) = Store::load(folder.path());
        assert_eq!(store.for_project(Some("/code/app")).notes, "project rule");

        // Turning its own set off puts the project back on the general settings.
        let (mut store, _) = Store::load(folder.path());
        let general = Settings { notes: "for everyone".into(), ..Settings::default() };
        store.save(Some("/code/app"), general, false).unwrap();
        let settings = store.for_project(Some("/code/app"));
        assert!(!settings.own);
        assert_eq!(settings.notes, "for everyone");
    }

    #[test]
    fn own_settings_need_a_project() {
        let folder = tempfile::tempdir().unwrap();
        let (mut store, _) = Store::load(folder.path());
        assert!(store.save(None, Settings::default(), true).is_err());
    }

    #[test]
    fn the_check_command_is_one_tidy_line() {
        let mut settings = Settings { check_command: "  cargo test  ".into(), ..Settings::default() };
        assert_eq!(settings.clone().validated().unwrap().check_command, "cargo test");
        settings.check_command = "a\nb".into();
        assert!(settings.clone().validated().is_err());
        settings.check_command = "x".repeat(MAX_CHECK_COMMAND_LENGTH + 1);
        assert!(settings.validated().is_err());
    }

    #[test]
    fn defaults_are_the_balanced_preset() {
        let settings = Settings::default();
        assert_eq!(settings, Settings::preset(Preset::Balanced));
        assert_eq!(settings.planner.model, "opus");
        assert_eq!(settings.worker.model, "sonnet");
        assert_eq!(settings.dispatcher.model, "haiku");
        assert_eq!(settings.reviewer.effort, Some(Effort::Medium));
        assert!(!settings.use_mcp_servers);
    }

    #[test]
    fn every_preset_is_valid() {
        for preset in Preset::ALL {
            assert!(Settings::preset(preset).validated().is_ok(), "{preset:?}");
        }
    }

    #[test]
    fn effort_is_written_in_lowercase() {
        assert_eq!(serde_json::to_string(&Effort::Xhigh).unwrap(), "\"xhigh\"");
        assert_eq!(Effort::Max.as_flag(), "max");
    }

    #[test]
    fn model_names_are_checked() {
        let mut settings = Settings::default();
        settings.worker.model = "  claude-sonnet-5-5  ".to_string();
        assert_eq!(settings.clone().validated().unwrap().worker.model, "claude-sonnet-5-5");

        for bad in ["", "--dangerously-skip-permissions", "opus; rm -rf ~", "a b"] {
            settings.worker.model = bad.to_string();
            assert!(settings.clone().validated().is_err(), "{bad:?} should be refused");
        }
        settings.worker.model = "opus[1m]".to_string();
        assert!(settings.validated().is_ok());
    }

    #[test]
    fn the_budget_must_be_sensible() {
        let mut settings = Settings::default();
        for bad in [0.0, -1.0, f64::NAN, MAX_BUDGET_LIMIT_USD + 1.0] {
            settings.max_budget_usd = Some(bad);
            assert!(settings.clone().validated().is_err(), "{bad} should be refused");
        }
        settings.max_budget_usd = None;
        assert!(settings.validated().is_ok());
    }

    #[test]
    fn saving_and_loading_round_trips() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("nested/settings.json");
        let mut settings = Settings::default();
        settings.planner.effort = Some(Effort::High);
        settings.reviewer.model = "haiku".to_string();
        settings.max_budget_usd = None;

        save(&path, &settings).unwrap();
        assert_eq!(load(&path), (settings, None));
    }

    #[test]
    fn a_missing_file_gives_the_defaults_quietly() {
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(load(&folder.path().join("nope.json")), (Settings::default(), None));
    }

    #[test]
    fn a_broken_file_gives_the_defaults_with_a_message() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.json");
        fs::write(&path, "{ not json").unwrap();
        let (settings, message) = load(&path);
        assert_eq!(settings, Settings::default());
        assert!(message.unwrap().contains("using the defaults"));
    }

    #[test]
    fn a_file_missing_new_fields_still_loads() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.json");
        fs::write(&path, r#"{"planner": {"model": "sonnet"}}"#).unwrap();
        let (settings, message) = load(&path);
        assert!(message.is_none());
        assert_eq!(settings.planner.model, "sonnet");
        assert_eq!(settings.worker, Settings::default().worker);
        assert!(!settings.use_mcp_servers);
        assert_eq!(settings.allowed_commands, default_commands(), "new fields take their defaults");
        assert_eq!(settings.worker_count, WORKER_COUNT);
    }

    #[test]
    fn allowed_commands_are_tidied_and_checked() {
        let mut settings = Settings {
            allowed_commands: vec![
                "  npm test  ".into(),
                "".into(),
                "npm test".into(),
                "npm run lint *".into(),
            ],
            ..Settings::default()
        };
        let tidy = settings.clone().validated().unwrap();
        assert_eq!(tidy.allowed_commands, ["npm test", "npm run lint *"]);

        for bad in ["*", " * ", "npm test, rm -rf ~", "Bash(rm)", "a\nb"] {
            settings.allowed_commands = vec![bad.to_string()];
            assert!(settings.clone().validated().is_err(), "{bad:?} should be refused");
        }
        settings.allowed_commands = Vec::new();
        assert!(settings.validated().is_ok(), "an empty list takes the shell away");
    }

    #[test]
    fn the_default_commands_are_the_check_commands() {
        let commands = default_commands();
        assert_eq!(commands.len(), CHECK_COMMANDS.len());
        assert!(commands.contains(&"cargo test *".to_string()));
        assert!(Settings::default().validated().is_ok());
    }

    #[test]
    fn team_size_turns_and_notes_are_checked() {
        let mut settings = Settings::default();
        for bad in [0, WORKER_COUNT + 1] {
            settings.worker_count = bad;
            assert!(settings.clone().validated().is_err(), "{bad} workers");
        }
        settings.worker_count = 1;
        settings.planner.max_turns = Some(0);
        assert!(settings.clone().validated().is_err());
        settings.planner.max_turns = Some(40);
        settings.chat_max_turns = Some(MAX_TURNS_LIMIT + 1);
        assert!(settings.clone().validated().is_err());
        settings.chat_max_turns = None;
        settings.notes = "x".repeat(MAX_NOTES_LENGTH + 1);
        assert!(settings.clone().validated().is_err());
        settings.notes = "  Use uv.  ".into();
        assert_eq!(settings.validated().unwrap().notes, "Use uv.");
    }

    #[test]
    fn settings_saved_under_the_old_role_name_still_load() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.json");
        fs::write(&path, r#"{"jarvis": {"model": "sonnet"}}"#).unwrap();
        assert_eq!(load(&path).0.dispatcher.model, "sonnet");
    }
}
