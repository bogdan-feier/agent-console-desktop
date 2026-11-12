//! Git worktrees: a separate copy of the project for each worker.
//!
//! A git worktree is a second (third, fourth...) folder attached to the same
//! repository, with its own branch checked out. Workers in different worktrees
//! can edit files at the same time without touching each other's copies or your
//! project folder. One round of work looks like this:
//!
//! ```text
//! your project (clean, at commit B)
//!   |-- worktree worker-1  on branch agent-console/<run>-round1/worker-1  (from B)
//!   |-- worktree worker-2  on branch .../worker-2                          (from B)
//!   |-- worktree worker-3  on branch .../worker-3                          (from B)
//!   '-- worktree review    on branch .../review: every worker branch merged in
//! ```
//!
//! When a worker finishes, its changes are committed on its own branch. The review
//! worktree merges all worker branches, and the reviewer checks the result there.
//! **Apply** squash-merges the review branch into your project: the changes end up
//! staged but not committed, so you look at them and commit them yourself.
//! **Discard** deletes the worktrees and branches. Your project folder is never
//! touched until Apply.
//!
//! The worktrees live in the system's temporary folder, outside your project.
//! Everything here runs the `git` program; nothing edits `.git` directly.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::changes::{Changes, MAX_CHANGES_CHARS};

/// Who the app's own commits on the temporary branches are from. They never reach
/// your history: Apply squashes them into staged changes that you commit yourself.
const COMMIT_SETTINGS: &[&str] = &[
    "-c",
    "user.name=Agent Console",
    "-c",
    "user.email=agent-console@localhost",
    // Signing would ask for a key or a passphrase that nobody is there to give.
    "-c",
    "commit.gpgsign=false",
];
const REVIEW: &str = "review";

/// The worktrees and branches of one round of work.
#[derive(Debug)]
pub struct Workspace {
    project_dir: PathBuf,
    /// Folder holding all of this round's worktrees.
    root: PathBuf,
    /// The commit every worktree started from.
    base: String,
    /// Start of every branch name of this round.
    branch_prefix: String,
    workers: usize,
}

/// What a worker changed, once committed on its branch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorkerCommit {
    /// Every file the worker created, changed or deleted.
    pub changed: Vec<String>,
    /// The ones that were not on the worker's list of files.
    pub outside_task: Vec<String>,
}

impl Workspace {
    /// Check the project and create one worktree per worker plus the review worktree.
    pub async fn create(
        project_dir: &Path,
        run_id: &str,
        round: usize,
        workers: usize,
    ) -> Result<Workspace, String> {
        if git(project_dir, &["rev-parse", "--is-inside-work-tree"]).await.is_err() {
            return Err("The workers each get their own copy of the project through git worktrees, so the \
                        project must be a git repository. Run `git init` in it and make a first commit."
                .to_string());
        }
        let base = git(project_dir, &["rev-parse", "--verify", "HEAD"])
            .await
            .map_err(|_| "The project has no commits yet. Make a first commit so the workers have something to start from.".to_string())?
            .trim()
            .to_string();
        let changes = git(project_dir, &["status", "--porcelain"]).await?;
        let changed_files = changes.lines().filter(|line| !line.trim().is_empty()).count();
        if changed_files > 0 {
            return Err(format!(
                "The project has uncommitted changes ({changed_files} files). Commit or stash them first: the \
                 workers start from the last commit, and Apply needs a clean folder to put their work in."
            ));
        }
        // Forget worktrees whose folders no longer exist (from an app that was force-quit).
        let _ = git(project_dir, &["worktree", "prune"]).await;

        let name = format!("{run_id}-round{round}");
        let workspace = Workspace {
            project_dir: project_dir.to_path_buf(),
            root: std::env::temp_dir().join("agent-console").join(&name),
            base,
            branch_prefix: format!("agent-console/{name}"),
            workers,
        };
        for part in workspace.parts() {
            let (branch, dir) = (workspace.branch(&part), workspace.root.join(&part));
            let created =
                git(project_dir, &["worktree", "add", "-q", "-b", &branch, &path_arg(&dir), &workspace.base])
                    .await;
            if let Err(problem) = created {
                workspace.remove().await;
                return Err(format!("Could not create the worktree for {part}: {problem}"));
            }
        }
        Ok(workspace)
    }

    /// "worker-1", "worker-2", ..., "review".
    fn parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = (1..=self.workers).map(|number| format!("worker-{number}")).collect();
        parts.push(REVIEW.to_string());
        parts
    }

    fn branch(&self, part: &str) -> String {
        format!("{}/{part}", self.branch_prefix)
    }

    pub fn worker_dir(&self, number: usize) -> PathBuf {
        self.root.join(format!("worker-{number}"))
    }

    pub fn review_dir(&self) -> PathBuf {
        self.root.join(REVIEW)
    }

    /// The branch Apply merges, for doing it by hand.
    pub fn review_branch(&self) -> String {
        self.branch(REVIEW)
    }

    /// Commit everything worker `number` changed onto its branch.
    ///
    /// `task_files` are the files the plan gave the worker; anything else it changed
    /// is reported in `outside_task`, because it may clash with another worker.
    pub async fn commit_worker(
        &self,
        number: usize,
        title: &str,
        task_files: &[String],
    ) -> Result<WorkerCommit, String> {
        let dir = self.worker_dir(number);
        git(&dir, &["add", "-A"]).await?;
        let staged = git(&dir, &["diff", "--cached", "--name-only"]).await?;
        let changed: Vec<String> =
            staged.lines().filter(|line| !line.is_empty()).map(str::to_string).collect();
        if changed.is_empty() {
            return Ok(WorkerCommit::default());
        }
        let message = format!("worker {number}: {title}");
        // --no-verify skips the project's commit hooks for these temporary commits;
        // they run when you commit the applied changes yourself.
        let commit = [COMMIT_SETTINGS, &["commit", "-q", "--no-verify", "-m", &message]].concat();
        git(&dir, &commit).await?;
        let outside_task = changed.iter().filter(|file| !task_files.contains(file)).cloned().collect();
        Ok(WorkerCommit { changed, outside_task })
    }

    /// Merge the branches of these workers into the review worktree. A branch that
    /// is already merged is skipped by git. On a clash, the merge is undone and the
    /// error names the files.
    pub async fn merge_into_review(&self, workers: &[usize]) -> Result<(), String> {
        let dir = self.review_dir();
        for &number in workers {
            let branch = self.branch(&format!("worker-{number}"));
            let merge =
                [COMMIT_SETTINGS, &["merge", "-q", "--no-ff", "--no-edit", "--no-verify", &branch]].concat();
            if git(&dir, &merge).await.is_err() {
                let clashing =
                    git(&dir, &["diff", "--name-only", "--diff-filter=U"]).await.unwrap_or_default();
                let _ = git(&dir, &["merge", "--abort"]).await;
                let files = clashing.lines().collect::<Vec<_>>().join(", ");
                return Err(format!(
                    "Worker {number}'s changes clash with another worker's in: {files}. Two tasks changed the \
                     same file. Send one worker back to undo its change there, or discard and plan again."
                ));
            }
        }
        Ok(())
    }

    /// The commit the review worktree is at.
    pub async fn review_commit(&self) -> Result<String, String> {
        Ok(git(&self.review_dir(), &["rev-parse", "HEAD"]).await?.trim().to_string())
    }

    /// Everything merged into the review worktree since `since` (or since the round
    /// started), as text for the reviewer's prompt.
    pub async fn review_changes(&self, since: Option<&str>) -> Changes {
        let range = format!("{}..HEAD", since.unwrap_or(&self.base));
        match git(&self.review_dir(), &["diff", &range]).await {
            Err(_) => Changes::Unknown,
            Ok(diff) if diff.trim().is_empty() => Changes::Nothing,
            Ok(diff) if diff.chars().count() > MAX_CHANGES_CHARS => Changes::TooLong,
            Ok(diff) => Changes::Diff(diff),
        }
    }

    /// Put the reviewed changes into the project folder as staged, uncommitted
    /// changes. Returns the files that changed.
    pub async fn apply(&self) -> Result<Vec<String>, String> {
        let project = &self.project_dir;
        if !git(project, &["status", "--porcelain"]).await?.trim().is_empty() {
            return Err(
                "Your project folder has uncommitted changes now. Commit or stash them, then apply again."
                    .to_string(),
            );
        }
        let head = git(project, &["rev-parse", "HEAD"]).await?.trim().to_string();
        if head != self.base {
            return Err(format!(
                "Your project has new commits since the workers started, so the changes are not applied \
                 automatically. To bring them in yourself, run: git merge --squash {}",
                self.review_branch()
            ));
        }
        git(project, &["merge", "--squash", "-q", &self.review_branch()]).await?;
        let staged = git(project, &["diff", "--cached", "--name-only"]).await?;
        Ok(staged.lines().filter(|line| !line.is_empty()).map(str::to_string).collect())
    }

    /// Delete this round's worktrees and branches. Safe to call more than once.
    pub async fn remove(&self) {
        for part in self.parts() {
            let dir = path_arg(&self.root.join(&part));
            let _ = git(&self.project_dir, &["worktree", "remove", "--force", &dir]).await;
            let _ = git(&self.project_dir, &["branch", "-q", "-D", &self.branch(&part)]).await;
        }
        let _ = tokio::fs::remove_dir_all(&self.root).await;
        let _ = git(&self.project_dir, &["worktree", "prune"]).await;
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Run git in `dir`. Returns what it printed, or what it complained about.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        // Never stop to ask for a password or an editor: nobody is there to answer.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Could not run git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git is installed");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        fs::write(dir.path().join("b.py"), "y = 1\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "start"]);
        dir
    }

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn run_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    }

    #[tokio::test]
    async fn a_whole_round_from_worktrees_to_applied_changes() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 2).await.unwrap();
        assert!(workspace.worker_dir(1).join("a.py").exists(), "each worktree is a full copy");

        // The workers edit their own copies; the project folder does not change.
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 2\n").unwrap();
        fs::write(workspace.worker_dir(2).join("c.py"), "z = 3\n").unwrap();
        fs::write(workspace.worker_dir(2).join("notes.txt"), "scratch\n").unwrap();
        assert_eq!(fs::read_to_string(project.path().join("a.py")).unwrap(), "x = 1\n");

        let one = workspace.commit_worker(1, "change a", &files(&["a.py"])).await.unwrap();
        assert_eq!(one, WorkerCommit { changed: files(&["a.py"]), outside_task: vec![] });
        let two = workspace.commit_worker(2, "add c", &files(&["c.py"])).await.unwrap();
        assert_eq!(two.outside_task, files(&["notes.txt"]));

        workspace.merge_into_review(&[1, 2]).await.unwrap();
        let Changes::Diff(diff) = workspace.review_changes(None).await else { panic!("expected a diff") };
        assert!(diff.contains("+x = 2") && diff.contains("+z = 3"));

        // A second review only sees what changed after the first one.
        let reviewed = workspace.review_commit().await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 3\n").unwrap();
        workspace.commit_worker(1, "fix a", &files(&["a.py"])).await.unwrap();
        workspace.merge_into_review(&[1, 2]).await.unwrap();
        let Changes::Diff(since) = workspace.review_changes(Some(&reviewed)).await else {
            panic!("expected a diff")
        };
        assert!(since.contains("+x = 3") && !since.contains("z = 3"));

        let applied = workspace.apply().await.unwrap();
        assert_eq!(applied, files(&["a.py", "c.py", "notes.txt"]));
        assert_eq!(fs::read_to_string(project.path().join("a.py")).unwrap(), "x = 3\n");
        let staged = run_git(project.path(), &["status", "--porcelain"]);
        assert!(staged.contains("M  a.py") && staged.contains("A  c.py"), "staged, not committed: {staged}");
        assert_eq!(run_git(project.path(), &["log", "--oneline"]).lines().count(), 1, "no commits added");

        workspace.remove().await;
        assert_eq!(run_git(project.path(), &["worktree", "list"]).lines().count(), 1);
        assert!(run_git(project.path(), &["branch", "--list", "agent-console/*"]).trim().is_empty());
        assert!(!workspace.root.exists());
    }

    #[tokio::test]
    async fn a_worker_that_changed_nothing_makes_no_commit() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        assert_eq!(
            workspace.commit_worker(1, "t", &files(&["a.py"])).await.unwrap(),
            WorkerCommit::default()
        );
        workspace.merge_into_review(&[1]).await.unwrap();
        assert_eq!(workspace.review_changes(None).await, Changes::Nothing);
        workspace.remove().await;
    }

    #[tokio::test]
    async fn two_workers_changing_the_same_file_is_reported_as_a_clash() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 2).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 'one'\n").unwrap();
        fs::write(workspace.worker_dir(2).join("a.py"), "x = 'two'\n").unwrap();
        workspace.commit_worker(1, "t1", &files(&["a.py"])).await.unwrap();
        workspace.commit_worker(2, "t2", &files(&["a.py"])).await.unwrap();

        let problem = workspace.merge_into_review(&[1, 2]).await.unwrap_err();
        assert!(problem.contains("Worker 2") && problem.contains("a.py"), "{problem}");
        // The failed merge was undone: the review worktree has worker 1's version.
        assert_eq!(fs::read_to_string(workspace.review_dir().join("a.py")).unwrap(), "x = 'one'\n");
        workspace.remove().await;
    }

    #[tokio::test]
    async fn the_project_must_be_a_clean_repository_with_a_commit() {
        let not_git = tempfile::tempdir().unwrap();
        let problem = Workspace::create(not_git.path(), &run_id(), 1, 1).await.unwrap_err();
        assert!(problem.contains("git init"), "{problem}");

        let empty = tempfile::tempdir().unwrap();
        run_git(empty.path(), &["init", "-q"]);
        let problem = Workspace::create(empty.path(), &run_id(), 1, 1).await.unwrap_err();
        assert!(problem.contains("no commits"), "{problem}");

        let dirty = project();
        fs::write(dirty.path().join("a.py"), "edited\n").unwrap();
        let problem = Workspace::create(dirty.path(), &run_id(), 1, 1).await.unwrap_err();
        assert!(problem.contains("uncommitted changes (1 files)"), "{problem}");
    }

    #[tokio::test]
    async fn apply_refuses_when_the_project_moved_on() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 2\n").unwrap();
        workspace.commit_worker(1, "t", &files(&["a.py"])).await.unwrap();
        workspace.merge_into_review(&[1]).await.unwrap();

        fs::write(project.path().join("b.py"), "y = 2\n").unwrap();
        let problem = workspace.apply().await.unwrap_err();
        assert!(problem.contains("uncommitted"), "{problem}");

        run_git(project.path(), &["commit", "-qam", "my own work"]);
        let problem = workspace.apply().await.unwrap_err();
        assert!(problem.contains("git merge --squash agent-console/"), "{problem}");
        workspace.remove().await;
    }
}
