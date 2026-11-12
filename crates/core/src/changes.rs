//! What a worker changed, as text for the reviewer's prompt.
//!
//! Handing the reviewer the diff up front saves it several turns of running
//! `git diff` and reading whole files, and each of those turns would resend
//! everything read so far.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// Longest diff put into a prompt, in characters. Past this, the reviewer reads the
/// files itself, which is cheaper than pasting a huge diff.
pub const MAX_CHANGES_CHARS: usize = 30_000;

/// What happened when looking for the changes to a task's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changes {
    /// The diff, plus the full text of any new files.
    Diff(String),
    /// Git shows no changes to the task's files.
    Nothing,
    /// The changes are longer than `MAX_CHANGES_CHARS`.
    TooLong,
    /// Not a git repository (or git is missing), so the changes are unknown.
    Unknown,
}

/// The changes to `files` (paths relative to `project_dir`) since the last commit.
pub async fn changes_to(project_dir: &Path, files: &[String]) -> Changes {
    if files.is_empty() || git(project_dir, &["rev-parse", "--is-inside-work-tree"], &[]).await.is_none() {
        return Changes::Unknown;
    }
    let mut text = String::new();

    // Files git already tracks: their diff against the last commit. In a repository
    // with no commits yet this fails, and every file is new (handled below).
    if let Some(diff) = git(project_dir, &["diff", "HEAD", "--"], files).await {
        text.push_str(&diff);
    }
    // Files git does not track yet: show them whole. `git ls-files` only lists paths
    // inside the repository, so nothing outside the project can end up in a prompt.
    if let Some(new_files) =
        git(project_dir, &["ls-files", "--others", "--exclude-standard", "--"], files).await
    {
        for path in new_files.lines().filter(|line| !line.is_empty()) {
            if let Ok(content) = tokio::fs::read_to_string(project_dir.join(path)).await {
                text.push_str(&format!("\n+++ new file: {path}\n{content}\n"));
            }
        }
    }

    if text.trim().is_empty() {
        Changes::Nothing
    } else if text.chars().count() > MAX_CHANGES_CHARS {
        Changes::TooLong
    } else {
        Changes::Diff(text)
    }
}

/// Run git with `args`, then `paths`. Returns its output, or `None` if it failed.
async fn git(project_dir: &Path, args: &[&str], paths: &[String]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .args(paths)
        .current_dir(project_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git is installed");
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo_with_one_commit() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        fs::write(dir.path().join("other.py"), "y = 1\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "start"]);
        dir
    }

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[tokio::test]
    async fn changed_and_new_files_of_the_task_are_included() {
        let dir = repo_with_one_commit();
        fs::write(dir.path().join("a.py"), "x = 2\n").unwrap();
        fs::write(dir.path().join("test_a.py"), "def test(): pass\n").unwrap();
        fs::write(dir.path().join("other.py"), "y = 99\n").unwrap();

        let Changes::Diff(text) = changes_to(dir.path(), &files(&["a.py", "test_a.py"])).await else {
            panic!("expected a diff")
        };
        assert!(text.contains("-x = 1") && text.contains("+x = 2"));
        assert!(text.contains("+++ new file: test_a.py") && text.contains("def test()"));
        assert!(!text.contains("y = 99"), "files outside the task are left out");
    }

    #[tokio::test]
    async fn untouched_files_mean_nothing_changed() {
        let dir = repo_with_one_commit();
        assert_eq!(changes_to(dir.path(), &files(&["a.py"])).await, Changes::Nothing);
    }

    #[tokio::test]
    async fn new_files_in_a_repository_without_commits_are_included() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let Changes::Diff(text) = changes_to(dir.path(), &files(&["a.py"])).await else {
            panic!("expected a diff")
        };
        assert!(text.contains("new file: a.py"));
    }

    #[tokio::test]
    async fn a_huge_diff_is_not_pasted() {
        let dir = repo_with_one_commit();
        fs::write(dir.path().join("a.py"), "z".repeat(MAX_CHANGES_CHARS + 10)).unwrap();
        assert_eq!(changes_to(dir.path(), &files(&["a.py"])).await, Changes::TooLong);
    }

    #[tokio::test]
    async fn outside_a_repository_the_changes_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        assert_eq!(changes_to(dir.path(), &files(&["a.py"])).await, Changes::Unknown);
        assert_eq!(changes_to(dir.path(), &[]).await, Changes::Unknown);
    }

    #[tokio::test]
    async fn a_file_name_that_looks_like_an_option_is_treated_as_a_file() {
        let dir = repo_with_one_commit();
        assert_eq!(changes_to(dir.path(), &files(&["--output=/tmp/x"])).await, Changes::Nothing);
    }
}
