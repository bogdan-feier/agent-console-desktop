//! Builds the command line for one `claude` session.
//!
//! These are the same flags the official Agent SDK passes. Run `claude --help`
//! to see what each one does.

use crate::config::AgentSpec;

pub fn build_args(spec: &AgentSpec, resume: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        // -p: print mode. Run one prompt without the interactive screen. The
        // prompt itself is sent on stdin, so it never hits an argument-length limit.
        "-p",
        // One JSON object per line as things happen (needs --verbose).
        "--output-format",
        "stream-json",
        "--verbose",
        // Anything not pre-approved is refused instead of waiting for a person
        // to click "allow". Nobody is there to click.
        "--permission-mode",
        "dontAsk",
    ]
    .map(String::from)
    .to_vec();

    let mut flag = |name: &str, value: String| {
        args.push(name.to_string());
        args.push(value);
    };
    flag("--model", spec.model.clone());
    if let Some(effort) = spec.effort {
        flag("--effort", effort.as_flag().to_string());
    }
    flag("--tools", spec.tools.join(","));
    flag("--allowedTools", spec.allowed.join(","));
    // Keeps Claude Code's normal system prompt and adds this agent's role on top.
    flag("--append-system-prompt", spec.instructions.to_string());
    if let Some(budget) = spec.max_budget_usd {
        flag("--max-budget-usd", budget.to_string());
    }
    if let Some(schema) = &spec.output_schema {
        flag("--json-schema", schema.to_string());
    }
    if let Some(session) = resume {
        // The "=" form keeps the value attached to the flag even if it starts with a dash.
        args.push(format!("--resume={session}"));
    }
    args
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

    #[test]
    fn planner_is_read_only_opus_with_a_schema() {
        let args = build_args(&config::planner(&Settings::default()), None);
        assert_eq!(value_after(&args, "--model"), "opus");
        assert_eq!(value_after(&args, "--tools"), "Read,Grep,Glob");
        assert_eq!(value_after(&args, "--permission-mode"), "dontAsk");
        assert!(value_after(&args, "--json-schema").contains("\"tasks\""));
        assert!(!args.iter().any(|arg| arg.starts_with("--resume")));
        assert!(!args.contains(&"--effort".to_string()), "no effort set means no flag");
        assert_eq!(value_after(&args, "--max-budget-usd"), "5");
    }

    #[test]
    fn effort_and_budget_follow_the_settings() {
        let mut settings = Settings::default();
        settings.worker.effort = Some(Effort::Low);
        settings.max_budget_usd = None;
        let args = build_args(&config::worker(&settings, 2), None);
        assert_eq!(value_after(&args, "--effort"), "low");
        assert!(!args.contains(&"--max-budget-usd".to_string()));
    }

    #[test]
    fn worker_can_edit_and_only_run_listed_commands() {
        let args = build_args(&config::worker(&Settings::default(), 1), Some("abc-123"));
        assert_eq!(value_after(&args, "--model"), "sonnet");
        assert_eq!(value_after(&args, "--tools"), "Read,Grep,Glob,Edit,Write,Bash");
        let allowed = value_after(&args, "--allowedTools");
        assert!(allowed.contains("Edit") && allowed.contains("Bash(cargo test *)"));
        // Plain "Bash" would allow any command; it must only appear as specific rules.
        assert!(!allowed.split(',').any(|rule| rule == "Bash"));
        assert!(args.contains(&"--resume=abc-123".to_string()));
        assert!(!args.contains(&"--json-schema".to_string()));
    }
}
