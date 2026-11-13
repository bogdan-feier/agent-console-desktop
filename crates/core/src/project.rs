//! Looking at a project to guess what its agents will need to run.
//!
//! Every project has a handful of commands that are obviously fine for an agent
//! to run in it: a Rust project needs `cargo test`, a Node one `npm test`. Typing
//! those into Settings by hand is a chore and easy to get wrong, so the app offers
//! them. It only ever offers; what ends up allowed is still the user's choice,
//! made in the Settings window before saving.
//!
//! Nothing here runs anything. It reads a few well-known files and nothing else.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// What a project looks like it needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Suggestions {
    /// Allowed-command rules, in the form the Settings window uses.
    pub commands: Vec<String>,
    /// A command the app itself can run on the merged changes, when there is an
    /// obvious one.
    pub check: Option<String>,
    /// What was recognised ("Rust", "Node"), for the line that says what was added.
    pub kinds: Vec<String>,
}

impl Suggestions {
    fn add(&mut self, kind: &str, commands: &[&str], check: &str) {
        self.kinds.push(kind.to_string());
        for command in commands {
            let command = command.to_string();
            if !self.commands.contains(&command) {
                self.commands.push(command);
            }
        }
        // The first thing recognised decides the check command: in a project with
        // both, the one whose files are at the top is usually the main one.
        if self.check.is_none() {
            self.check = Some(check.to_string());
        }
    }
}

/// Look at `dir` and say what its agents will probably need. An unrecognised
/// project gets nothing, which is the right answer: an empty list refuses
/// everything, and a guess that is wrong here would be a permission granted for
/// no reason.
pub fn suggestions(dir: &Path) -> Suggestions {
    let has = |name: &str| dir.join(name).exists();
    let mut found = Suggestions::default();

    if has("Cargo.toml") {
        found.add("Rust", &["cargo test *", "cargo build *", "cargo clippy *", "cargo fmt *"], "cargo test");
    }
    if has("package.json") {
        let runner = if has("pnpm-lock.yaml") {
            "pnpm"
        } else if has("yarn.lock") {
            "yarn"
        } else {
            "npm"
        };
        let scripts = package_scripts(dir);
        // Only what this project actually has: a rule for a script that does not
        // exist is a permission granted for nothing.
        let mut commands = vec![format!("{runner} run *"), format!("{runner} test *")];
        if scripts.iter().any(|name| name == "typecheck") {
            commands.push(format!("{runner} run typecheck *"));
        }
        commands.push("node *".to_string());
        let borrowed: Vec<&str> = commands.iter().map(String::as_str).collect();
        let check =
            if scripts.iter().any(|name| name == "test") { format!("{runner} test") } else { String::new() };
        found.add("Node", &borrowed, &check);
    }
    if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        if has("uv.lock") {
            found.add("Python (uv)", &["uv run *", "uv sync *", "uv pip *"], "uv run pytest -q");
        } else {
            found.add("Python", &["pytest *", "python -m pytest *", "python *"], "pytest -q");
        }
    }
    if has("go.mod") {
        found.add("Go", &["go test *", "go build *", "go vet *"], "go test ./...");
    }
    if has("Makefile") {
        found.add("Make", &["make *"], "make test");
    }

    // A check command of "" means the project had no obvious one.
    if found.check.as_deref() == Some("") {
        found.check = None;
    }
    found
}

/// The names in `package.json`'s "scripts". A file that will not parse gives
/// none, which only means fewer suggestions.
fn package_scripts(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else { return Vec::new() };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    match parsed.get("scripts").and_then(Value::as_object) {
        Some(scripts) => scripts.keys().cloned().collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn a_rust_project_gets_cargo() {
        let dir = project(&[("Cargo.toml", "[package]\nname = \"x\"\n")]);
        let found = suggestions(dir.path());
        assert_eq!(found.kinds, ["Rust"]);
        assert!(found.commands.contains(&"cargo test *".to_string()));
        assert_eq!(found.check.as_deref(), Some("cargo test"));
    }

    #[test]
    fn a_node_project_follows_its_lockfile_and_its_scripts() {
        let package = r#"{ "scripts": { "test": "vitest", "typecheck": "tsc" } }"#;
        let dir = project(&[("package.json", package), ("pnpm-lock.yaml", "")]);
        let found = suggestions(dir.path());
        assert!(found.commands.contains(&"pnpm run *".to_string()));
        assert!(found.commands.contains(&"pnpm run typecheck *".to_string()));
        assert!(!found.commands.iter().any(|rule| rule.starts_with("npm")));
        assert_eq!(found.check.as_deref(), Some("pnpm test"));
    }

    #[test]
    fn a_node_project_with_no_test_script_gets_no_check_command() {
        let dir = project(&[("package.json", r#"{ "scripts": { "build": "tsc" } }"#)]);
        let found = suggestions(dir.path());
        assert!(found.commands.contains(&"npm run *".to_string()));
        assert_eq!(found.check, None, "there is nothing obvious to run");
    }

    #[test]
    fn python_with_uv_is_told_apart_from_python_without() {
        let plain = project(&[("pyproject.toml", "")]);
        assert_eq!(suggestions(plain.path()).check.as_deref(), Some("pytest -q"));

        let with_uv = project(&[("pyproject.toml", ""), ("uv.lock", "")]);
        let found = suggestions(with_uv.path());
        assert!(found.commands.contains(&"uv run *".to_string()));
        assert_eq!(found.check.as_deref(), Some("uv run pytest -q"));
    }

    #[test]
    fn a_project_with_two_kinds_keeps_both_sets_of_commands() {
        let dir = project(&[("Cargo.toml", ""), ("package.json", r#"{ "scripts": {} }"#)]);
        let found = suggestions(dir.path());
        assert_eq!(found.kinds, ["Rust", "Node"]);
        assert!(found.commands.contains(&"cargo test *".to_string()));
        assert!(found.commands.contains(&"npm run *".to_string()));
        assert_eq!(found.check.as_deref(), Some("cargo test"), "the first one recognised");
    }

    #[test]
    fn an_unrecognised_project_is_offered_nothing() {
        let dir = project(&[("notes.txt", "hello")]);
        assert_eq!(suggestions(dir.path()), Suggestions::default());
    }
}
