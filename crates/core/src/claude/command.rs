//! Builds the command line for one `claude` session.
//!
//! Most of these are the same flags the official Agent SDK passes. Run
//! `claude --help` to see what each one does.

use std::collections::HashSet;

use crate::config::{AgentSpec, SKILL_TOOL};

/// Leave skills and slash commands out of the session. Passed only to agents without
/// the Skill tool, who could not use them anyway, so it only removes the skill
/// descriptions that would otherwise be sent on every turn.
pub const NO_SKILLS: &str = "--disable-slash-commands";
/// Use only the MCP servers given on the command line, which is none: no MCP
/// servers start, and none of their tool descriptions are sent.
pub const NO_MCP_SERVERS: &str = "--strict-mcp-config";
/// Move the parts of the system prompt that change between sessions (git status,
/// the date) into the first message. The system prompt then starts the same way in
/// every session, so one session can reuse what another put in the prompt cache.
pub const SHARED_SYSTEM_PROMPT: &str = "--exclude-dynamic-system-prompt-sections";
/// Read only the project's own settings, not the user's, so the plugins installed in the
/// user's Claude Code stay out; `--plugin-dir` then loads just the ones listed.
const PROJECT_SETTINGS_ONLY: &str = "--setting-sources";
const PLUGIN_DIR: &str = "--plugin-dir";
const EFFORT: &str = "--effort";
const DENIED: &str = "--disallowedTools";

/// `known_flags` is what this version of `claude` lists in `--help` (see
/// `locate.rs`); `None` means it is not known. The usage-saving flags above are only
/// passed when they are known to work, so an older Claude Code still runs.
pub fn build_args(
    spec: &AgentSpec,
    resume: Option<&str>,
    known_flags: Option<&HashSet<String>>,
) -> Vec<String> {
    let known = |flag: &str| known_flags.is_some_and(|flags| flags.contains(flag));
    let mut args = Vec::new();

    // -p: print mode. Run one prompt without the interactive screen. The prompt
    // itself is sent on stdin, so it never hits an argument-length limit.
    args.push("-p".to_string());
    // One JSON object per line as things happen (needs --verbose).
    push(&mut args, "--output-format", "stream-json");
    args.push("--verbose".to_string());
    // Anything not pre-approved is refused instead of waiting for a person to
    // click "allow". Nobody is there to click.
    push(&mut args, "--permission-mode", "dontAsk");

    push(&mut args, "--model", &spec.model);
    if let Some(effort) = spec.effort {
        // The user chose this level, so pass it unless this claude is known not to have it.
        if known_flags.is_none() || known(EFFORT) {
            push(&mut args, EFFORT, effort.as_flag());
        }
    }
    push(&mut args, "--tools", &spec.tools.join(","));
    push(&mut args, "--allowedTools", &spec.allowed.join(","));
    // Deny wins over allow in Claude Code, so this holds even where `allowed` says
    // "any command". Passed unless this claude is known not to have the flag.
    if !spec.denied.is_empty() && !deny_list_ignored(spec, known_flags) {
        push(&mut args, DENIED, &spec.denied.join(","));
    }
    // Keeps Claude Code's normal system prompt and adds this agent's role on top.
    push(&mut args, "--append-system-prompt", &spec.instructions);
    push(&mut args, "--max-turns", &spec.max_turns.to_string());
    if let Some(budget) = spec.max_budget_usd {
        push(&mut args, "--max-budget-usd", &budget.to_string());
    }
    if let Some(schema) = &spec.output_schema {
        push(&mut args, "--json-schema", &schema.to_string());
    }

    let uses_skills = spec.tools.iter().any(|tool| tool == SKILL_TOOL);
    if !uses_skills && known(NO_SKILLS) {
        args.push(NO_SKILLS.to_string());
    }
    if known(SHARED_SYSTEM_PROMPT) {
        args.push(SHARED_SYSTEM_PROMPT.to_string());
    }
    if !spec.use_mcp_servers && known(NO_MCP_SERVERS) {
        args.push(NO_MCP_SERVERS.to_string());
    }
    // Both or neither: dropping the user's plugins without loading the listed ones would
    // leave the agent with none.
    if !spec.plugin_dirs.is_empty() && known(PROJECT_SETTINGS_ONLY) && known(PLUGIN_DIR) {
        push(&mut args, PROJECT_SETTINGS_ONLY, "project,local");
        for folder in &spec.plugin_dirs {
            push(&mut args, PLUGIN_DIR, folder);
        }
    }

    if let Some(session) = resume {
        // The "=" form keeps the value attached to the flag even if it starts with a dash.
        args.push(format!("--resume={session}"));
    }
    args
}

/// True when this agent has commands it must never run, but this `claude` has no way
/// to be told about them. The caller says so out loud: a limit the user was promised
/// and is not getting is worth a line on the screen.
pub fn deny_list_ignored(spec: &AgentSpec, known_flags: Option<&HashSet<String>>) -> bool {
    !spec.denied.is_empty() && known_flags.is_some_and(|flags| !flags.contains(DENIED))
}

fn push(args: &mut Vec<String>, flag: &str, value: &str) {
    args.push(flag.to_string());
    args.push(value.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::settings::{Effort, Settings};

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let position = args.iter().position(|arg| arg == flag).expect(flag);
        &args[position + 1]
    }

    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|arg| arg == flag)
    }

    fn flags(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn all_known() -> HashSet<String> {
        [NO_SKILLS, NO_MCP_SERVERS, SHARED_SYSTEM_PROMPT, EFFORT].map(String::from).into()
    }

    #[test]
    fn planner_is_read_only_opus_with_a_schema() {
        let args = build_args(&config::planner(&Settings::default()), None, None);
        assert_eq!(value_after(&args, "--model"), "opus");
        assert_eq!(value_after(&args, "--tools"), "Read,Grep,Glob");
        assert_eq!(value_after(&args, "--permission-mode"), "dontAsk");
        assert_eq!(value_after(&args, "--max-turns"), config::PLANNER_MAX_TURNS.to_string());
        assert!(value_after(&args, "--json-schema").contains("\"tasks\""));
        assert!(!args.iter().any(|arg| arg.starts_with("--resume")));
        assert_eq!(value_after(&args, "--max-budget-usd"), "5");
    }

    #[test]
    fn worker_can_edit_and_only_run_listed_commands() {
        let args = build_args(&config::worker(&Settings::default(), 1), Some("abc-123"), None);
        assert_eq!(value_after(&args, "--model"), "sonnet");
        assert_eq!(value_after(&args, "--tools"), "Read,Grep,Glob,Edit,Write,Bash,Skill");
        let allowed = value_after(&args, "--allowedTools");
        assert!(allowed.contains("Edit") && allowed.contains("Bash(cargo test *)"));
        // Plain "Bash" would allow any command; it must only appear as specific rules.
        assert!(!allowed.split(',').any(|rule| rule == "Bash"));
        assert!(has(&args, "--resume=abc-123"));
        assert!(!has(&args, "--json-schema"));
    }

    #[test]
    fn effort_and_budget_follow_the_settings() {
        let mut settings = Settings::default();
        settings.worker.effort = Some(Effort::Low);
        settings.max_budget_usd = None;
        let args = build_args(&config::worker(&settings, 2), None, None);
        assert_eq!(value_after(&args, "--effort"), "low");
        assert!(!has(&args, "--max-budget-usd"));

        settings.worker.effort = None;
        assert!(!has(&build_args(&config::worker(&settings, 2), None, None), "--effort"));
    }

    #[test]
    fn saving_flags_are_passed_only_when_claude_lists_them() {
        let spec = config::reviewer(&Settings::default());

        let unknown = build_args(&spec, None, None);
        assert!(!has(&unknown, NO_SKILLS) && !has(&unknown, NO_MCP_SERVERS));

        let known = all_known();
        let args = build_args(&spec, None, Some(&known));
        assert!(has(&args, NO_SKILLS));
        assert!(has(&args, NO_MCP_SERVERS));
        assert!(has(&args, SHARED_SYSTEM_PROMPT));

        let older: HashSet<String> = [NO_SKILLS.to_string()].into();
        let args = build_args(&spec, None, Some(&older));
        assert!(has(&args, NO_SKILLS));
        assert!(!has(&args, SHARED_SYSTEM_PROMPT));
        // This claude is known not to have --effort, so it is left out rather than failing.
        assert!(!has(&args, EFFORT));
    }

    #[test]
    fn mcp_servers_load_when_the_settings_ask_for_them() {
        let settings = Settings { use_mcp_servers: true, ..Settings::default() };
        let args = build_args(&config::worker(&settings, 1), None, Some(&all_known()));
        assert!(!has(&args, NO_MCP_SERVERS));
    }

    #[test]
    fn listed_plugins_replace_the_installed_ones_for_workers_only() {
        let settings =
            Settings { worker_plugins: vec!["/p/one".into(), "/p/two".into()], ..Settings::default() };
        let known = flags(&[PROJECT_SETTINGS_ONLY, PLUGIN_DIR]);

        let worker = build_args(&config::worker(&settings, 1), None, Some(&known));
        assert_eq!(value_after(&worker, PROJECT_SETTINGS_ONLY), "project,local");
        let folders: Vec<&String> =
            worker.windows(2).filter(|pair| pair[0] == PLUGIN_DIR).map(|pair| &pair[1]).collect();
        assert_eq!(folders, ["/p/one", "/p/two"]);

        let planner = build_args(&config::planner(&settings), None, Some(&known));
        assert!(!has(&planner, PROJECT_SETTINGS_ONLY) && !has(&planner, PLUGIN_DIR));

        // A claude without --plugin-dir keeps the installed plugins rather than losing all.
        let older = build_args(&config::worker(&settings, 1), None, Some(&flags(&[PROJECT_SETTINGS_ONLY])));
        assert!(!has(&older, PROJECT_SETTINGS_ONLY));
    }

    #[test]
    fn only_workers_see_the_installed_skills() {
        let settings = Settings::default();
        let worker = build_args(&config::worker(&settings, 1), None, Some(&all_known()));
        assert!(!has(&worker, NO_SKILLS));
        assert!(value_after(&worker, "--allowedTools").split(',').any(|rule| rule == "Skill"));

        for spec in [config::planner(&settings), config::reviewer(&settings)] {
            let args = build_args(&spec, None, Some(&all_known()));
            assert!(has(&args, NO_SKILLS), "{}", spec.id);
            assert!(!value_after(&args, "--tools").contains("Skill"), "{}", spec.id);
        }
    }

    #[test]
    fn what_may_never_be_run_is_passed_along() {
        let settings = Settings { full_shell: true, ..Settings::default() };
        let args = build_args(&config::worker(&settings, 1), None, Some(&flags(&["--disallowedTools"])));

        let denied = value_after(&args, "--disallowedTools");
        assert!(denied.contains("Bash(git push *)"), "{denied}");
        assert!(value_after(&args, "--allowedTools").contains("Bash(*)"));
    }

    #[test]
    fn an_older_claude_that_cannot_refuse_is_not_handed_the_flag() {
        let settings = Settings { full_shell: true, ..Settings::default() };
        let spec = config::worker(&settings, 1);
        let old = flags(&["--allowedTools"]);

        assert!(deny_list_ignored(&spec, Some(&old)), "the caller must be able to say so");
        assert!(!build_args(&spec, None, Some(&old)).iter().any(|arg| arg == "--disallowedTools"));
    }

    #[test]
    fn an_agent_with_no_shell_has_nothing_to_refuse() {
        let spec = config::dispatcher(&Settings::default());
        assert!(!deny_list_ignored(&spec, Some(&flags(&["--allowedTools"]))));
    }
}
