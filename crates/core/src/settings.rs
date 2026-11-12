//! Settings you change in the app's Settings window: the model and effort for each
//! role, and the spending cap. They are saved as JSON so they survive a restart.
//!
//! The defaults come from the constants in `config.rs`.

use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::config::{
    DISPATCHER_MODEL, MAX_BUDGET_USD_PER_AGENT, PLANNER_MODEL, REVIEWER_MODEL, WORKER_MODEL,
};

/// Highest spending cap the Settings window accepts, in US dollars per agent session.
pub const MAX_BUDGET_LIMIT_USD: f64 = 1000.0;
const MAX_MODEL_NAME_LENGTH: usize = 100;

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

/// Model and effort for one role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleSettings {
    /// "opus", "sonnet", "haiku", or a full model ID.
    pub model: String,
    /// `None` means Claude Code's own default for that model.
    #[serde(default)]
    pub effort: Option<Effort>,
}

impl RoleSettings {
    fn new(model: &str, effort: Option<Effort>) -> Self {
        RoleSettings { model: model.to_string(), effort }
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
        Settings { dispatcher, planner, worker, reviewer, max_budget_usd: budget, use_mcp_servers: false }
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
    /// Check the settings and tidy them (trim model names). Errors are shown to the user.
    pub fn validated(mut self) -> Result<Settings, String> {
        for (label, role) in [
            ("Dispatcher", &mut self.dispatcher),
            ("Planner", &mut self.planner),
            ("Workers", &mut self.worker),
            ("Reviewer", &mut self.reviewer),
        ] {
            role.model = role.model.trim().to_string();
            check_model(&role.model).map_err(|problem| format!("{label}: {problem}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn settings_saved_under_the_old_role_name_still_load() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.json");
        fs::write(&path, r#"{"jarvis": {"model": "sonnet"}}"#).unwrap();
        assert_eq!(load(&path).0.dispatcher.model, "sonnet");
    }
}
