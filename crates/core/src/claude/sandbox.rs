//! Claude Code's sandbox for agents that may run any command.
//!
//! With "any command" on, a worker can run anything: delete, rename, install a
//! dependency. A list of forbidden commands cannot keep that inside the project
//! (`rm -rf` has too many spellings), so instead the operating system does:
//! Claude Code runs each shell command in macOS's sandbox with the limits below,
//! and refuses to run it at all if the sandbox cannot start.
//!
//! - Writes: the agent's own folder (and the temp folder), plus the git folder of
//!   its worktree, so `git rm` works. Not the rest of the repository's `.git`, whose
//!   config and hooks the app's own git commands would run outside the sandbox.
//!   Not the other workers' folders of the same round.
//! - Reads: everything except secrets: `.env` files and the usual credential folders.
//! - Network: package registries, so `uv add` or `npm install` work, and servers
//!   on this machine, so an agent can start the app it built and call it. No other
//!   host can be reached.
//!
//! Tried with a real `claude -p` session: deleting and renaming in the project and
//! `git rm` worked; writing in the home folder, a sibling worker's folder,
//! `.git/config` or `.git/hooks`, reading a denied file or `~/.ssh`, and reaching a
//! host off the list were all refused. Starting a server on 127.0.0.1 and calling it
//! with curl worked, and curl to example.com was refused.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Passes settings for one session only, on top of the project's own.
pub const SETTINGS_FLAG: &str = "--settings";

/// Hosts a package manager needs to add or update a dependency.
const PACKAGE_REGISTRIES: &[&str] = &[
    "pypi.org",
    "files.pythonhosted.org",
    "registry.npmjs.org",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
];

/// Caches package managers write to, outside the project.
const PACKAGE_CACHES: &[&str] = &["~/.cache/uv", "~/.npm", "~/.cargo"];

/// Credential folders no agent may read, by any means.
const SECRET_FOLDERS: &[&str] =
    &["~/.ssh", "~/.aws", "~/.gnupg", "~/.config/gh", "~/.docker", "~/.kube", "~/Library/Keychains"];

/// The same as permission rules: these also stop the Read tool, which does not run
/// in the sandbox, and Claude Code merges them into the sandbox's read limits.
const SECRET_READS: &[&str] = &["Read(**/.env)", "Read(**/.env.*)", "Read(~/.netrc)"];

/// The `--settings` value for one sandboxed session. `git_dir` is the agent's real
/// git folder, when it has one; `others` are folders it must not write to.
pub fn settings(git_dir: Option<&Path>, others: &[PathBuf]) -> Value {
    let mut allow_write: Vec<String> = PACKAGE_CACHES.iter().map(|path| path.to_string()).collect();
    if let Some(git_dir) = git_dir {
        allow_write.push(git_dir.display().to_string());
    }
    let deny_write: Vec<String> = others.iter().map(|path| path.display().to_string()).collect();
    json!({
        "sandbox": {
            "enabled": true,
            // Inside the sandbox every command is allowed without a rule for it.
            "autoAllowBashIfSandboxed": true,
            // A command the sandbox refuses must not be retried outside it.
            "allowUnsandboxedCommands": false,
            // No sandbox, no shell: never run "any command" unconfined.
            "failIfUnavailable": true,
            "filesystem": {
                "allowWrite": allow_write,
                "denyWrite": deny_write,
                "denyRead": SECRET_FOLDERS,
            },
            // The registries, so `uv add` works; and servers on this machine, so an
            // agent can start the app it built and try it. Nowhere else.
            "network": { "allowedDomains": PACKAGE_REGISTRIES, "allowLocalBinding": true },
        },
        "permissions": { "deny": SECRET_READS },
    })
}

/// The other folders of the same round as `folder`, when `folder` is one of the
/// app's worktrees: the temp folder they share is writable, so each is named.
pub fn other_round_folders(folder: &Path, app_root: &Path) -> Vec<PathBuf> {
    let Some(round) = folder.parent().filter(|round| round.starts_with(app_root)) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(round) else { return Vec::new() };
    entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir() && path != folder).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_keeps_writes_in_the_project_and_secrets_unread() {
        let value =
            settings(Some(Path::new("/repo/.git/worktrees/w1")), &[PathBuf::from("/t/round/worker-2")]);
        let sandbox = &value["sandbox"];
        assert_eq!(sandbox["enabled"], true);
        assert_eq!(sandbox["allowUnsandboxedCommands"], false, "never retried outside the sandbox");
        assert_eq!(sandbox["failIfUnavailable"], true, "never run unconfined");
        let allow_write = sandbox["filesystem"]["allowWrite"].as_array().unwrap();
        assert!(allow_write.contains(&json!("/repo/.git/worktrees/w1")), "only its own git folder");
        assert!(!allow_write.contains(&json!("/repo/.git")));
        assert_eq!(sandbox["filesystem"]["denyWrite"], json!(["/t/round/worker-2"]));
        assert!(sandbox["filesystem"]["denyRead"].as_array().unwrap().contains(&json!("~/.ssh")));
        assert!(sandbox["network"]["allowedDomains"].as_array().unwrap().contains(&json!("pypi.org")));
        assert_eq!(sandbox["network"]["allowLocalBinding"], true, "it may run the app it built");
        assert!(value["permissions"]["deny"].as_array().unwrap().contains(&json!("Read(**/.env)")));
    }

    #[test]
    fn only_the_apps_own_worktrees_fence_off_their_neighbours() {
        let root = tempfile::tempdir().unwrap();
        let round = root.path().join("run-round1");
        for name in ["worker-1", "worker-2", "review"] {
            std::fs::create_dir_all(round.join(name)).unwrap();
        }
        let mut others = other_round_folders(&round.join("worker-1"), root.path());
        others.sort();
        assert_eq!(others, [round.join("review"), round.join("worker-2")]);
        // A project folder outside the app's root has no neighbours to fence off.
        assert!(other_round_folders(&round.join("worker-1"), Path::new("/elsewhere")).is_empty());
    }
}
