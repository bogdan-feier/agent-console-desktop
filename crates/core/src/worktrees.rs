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

/// Longest patch the Changes window is given for one file. A patch longer than
/// this is cut off: the window is for reading, and nobody reads 400kB of diff.
pub const MAX_PATCH_CHARS: usize = 60_000;

/// How a merge into the review worktree went.
#[derive(Debug, PartialEq, Eq)]
pub enum Merge {
    /// Every branch went in. The review worktree holds all the work.
    Clean,
    /// Git stopped partway through `worker`'s branch: these files have changes on
    /// both sides of the same lines. The merge is still in progress in the review
    /// worktree, with the markers in the files, waiting to be resolved or aborted.
    Conflicted { worker: usize, files: Vec<String> },
}

/// One file's changes, for the window that shows a round before you apply it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileChange {
    pub path: String,
    /// Lines added and removed, for the summary next to the name.
    pub added: usize,
    pub removed: usize,
    /// The workers whose branches touched this file.
    pub workers: Vec<usize>,
    /// The `git diff` for this file alone.
    pub patch: String,
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
        if !changes.trim().is_empty() {
            return Err(dirty_message(&changes));
        }
        // Forget worktrees whose folders no longer exist (from an app that was
        // force-quit), and the branches that went with them.
        let _ = git(project_dir, &["worktree", "prune"]).await;
        clear_dead_branches(project_dir).await;

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

    /// The commit every worktree started from. Saved with a run so the round can
    /// be picked up again after the app is restarted.
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn worker_count(&self) -> usize {
        self.workers
    }

    /// The folder holding this round's worktrees.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// What all of this round's branch names start with.
    pub fn branch_prefix(&self) -> &str {
        &self.branch_prefix
    }

    /// Open a round's worktrees again, after a restart, from what was saved about
    /// them. Everything must still be there: the folders are under the system's
    /// temporary directory, which macOS clears out after a few days, and the user
    /// may have removed them by hand.
    pub async fn reopen(
        project_dir: &Path,
        run_id: &str,
        round: usize,
        workers: usize,
        base: &str,
    ) -> Result<Workspace, String> {
        let name = format!("{run_id}-round{round}");
        let workspace = Workspace {
            project_dir: project_dir.to_path_buf(),
            root: std::env::temp_dir().join("agent-console").join(&name),
            base: base.to_string(),
            branch_prefix: format!("agent-console/{name}"),
            workers,
        };
        for part in workspace.parts() {
            let dir = workspace.root.join(&part);
            // A folder that is still a git worktree answers this; a leftover
            // folder, or none at all, does not.
            if git(&dir, &["rev-parse", "--is-inside-work-tree"]).await.is_err() {
                return Err(format!("the copy of the project for {part} is no longer there"));
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

    /// Merge the branches of these workers into the review worktree, in order. A
    /// branch that is already merged is skipped by git, so this is safe to call
    /// again after a conflict has been resolved.
    ///
    /// On a clash the merge is **left in progress**, with git's markers in the
    /// files, and the caller decides: resolve it ([`finish_merge`]) or give up
    /// ([`abort_merge`]).
    pub async fn merge_into_review(&self, workers: &[usize]) -> Result<Merge, String> {
        let dir = self.review_dir();
        for &number in workers {
            let branch = self.branch(&format!("worker-{number}"));
            let merge =
                [COMMIT_SETTINGS, &["merge", "-q", "--no-ff", "--no-edit", "--no-verify", &branch]].concat();
            if git(&dir, &merge).await.is_err() {
                let files = self.unresolved().await;
                if files.is_empty() {
                    // It failed for some other reason; leave nothing half-done.
                    let _ = git(&dir, &["merge", "--abort"]).await;
                    return Err(format!("Could not merge worker {number}'s changes."));
                }
                return Ok(Merge::Conflicted { worker: number, files });
            }
        }
        Ok(Merge::Clean)
    }

    /// The files of the merge in progress that still have both sides in them.
    pub async fn unresolved(&self) -> Vec<String> {
        let listed = git(&self.review_dir(), &["diff", "--name-only", "--diff-filter=U"]).await;
        listed.unwrap_or_default().lines().filter(|line| !line.is_empty()).map(str::to_string).collect()
    }

    /// Any of `files` that still has a conflict marker in it. Git calls a file
    /// resolved as soon as it is staged, so this is the check that it really is.
    pub async fn still_marked(&self, files: &[String]) -> Vec<String> {
        let mut marked = Vec::new();
        for file in files {
            let full = self.review_dir().join(file);
            if let Ok(text) = tokio::fs::read_to_string(&full).await {
                let has_marker =
                    text.lines().any(|line| line.starts_with("<<<<<<<") || line.starts_with(">>>>>>>"));
                if has_marker {
                    marked.push(file.clone());
                }
            }
        }
        marked
    }

    /// Finish a merge whose conflicts have been resolved: stage everything and
    /// commit it. `files` are the ones that clashed; if any still has both
    /// versions in it, nothing is staged and nothing is committed.
    ///
    /// The marker check has to come first: `git add` counts a file as resolved the
    /// moment it is staged, markers and all.
    pub async fn finish_merge(&self, worker: usize, files: &[String], how: &str) -> Result<(), String> {
        let marked = self.still_marked(files).await;
        if !marked.is_empty() {
            return Err(format!("still has both versions in it: {}", marked.join(", ")));
        }
        let dir = self.review_dir();
        git(&dir, &["add", "-A"]).await?;
        let message = format!("merge worker {worker} ({how})");
        let commit = [COMMIT_SETTINGS, &["commit", "-q", "--no-verify", "-m", &message]].concat();
        git(&dir, &commit).await.map(|_| ())
    }

    /// Undo a merge that is in progress, putting the review worktree back as it was.
    pub async fn abort_merge(&self) {
        let _ = git(&self.review_dir(), &["merge", "--abort"]).await;
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

    /// Every file this round would change, with its own patch, for the window that
    /// shows the changes before they are applied.
    pub async fn file_changes(&self) -> Result<Vec<FileChange>, String> {
        let range = format!("{}..HEAD", self.base);
        let diff = git(&self.review_dir(), &["diff", &range]).await?;

        // Which worker touched what, so each file can say where it came from.
        let mut by_worker: Vec<(usize, Vec<String>)> = Vec::new();
        for number in 1..=self.workers {
            let branch = self.branch(&format!("worker-{number}"));
            let range = format!("{}..{branch}", self.base);
            if let Ok(listed) = git(&self.project_dir, &["diff", "--name-only", &range]).await {
                by_worker.push((number, listed.lines().map(str::to_string).collect()));
            }
        }

        Ok(split_patches(&diff)
            .into_iter()
            .map(|(path, patch)| {
                let workers = by_worker
                    .iter()
                    .filter(|(_, files)| files.contains(&path))
                    .map(|(number, _)| *number)
                    .collect();
                let (added, removed) = count_changed_lines(&patch);
                let patch = cut_to(patch, MAX_PATCH_CHARS);
                FileChange { path, added, removed, workers, patch }
            })
            .collect())
    }

    /// Put the reviewed changes into the project folder as staged, uncommitted
    /// changes. Returns the files that changed.
    ///
    /// The project folder must have nothing uncommitted in it: the merge below writes
    /// over the files it touches, and anything not committed would be gone for good.
    /// The project having moved on since the workers started is fine, though. Git works
    /// out the common ancestor itself, so the merge still means the right thing; it
    /// only fails if those new commits touched the same lines, and because everything
    /// is committed by this point, backing out of that loses nothing.
    pub async fn apply(&self) -> Result<Vec<String>, String> {
        let project = &self.project_dir;
        let dirty = git(project, &["status", "--porcelain"]).await?;
        if !dirty.trim().is_empty() {
            return Err(dirty_message(&dirty));
        }

        if let Err(clash) = git(project, &["merge", "--squash", "-q", &self.review_branch()]).await {
            // Leave the folder as it was. Safe: there was nothing uncommitted.
            let _ = git(project, &["reset", "--hard", "-q", "HEAD"]).await;
            return Err(format!(
                "Your project gained commits since the workers started, and they changed the same \
                 lines as this round, so the two could not be put together ({}). Your project is \
                 untouched. Ask the workers to redo their task against the new code, or discard the \
                 round.",
                why(clash)
            ));
        }
        let staged = git(project, &["diff", "--cached", "--name-only"]).await?;
        Ok(staged.lines().filter(|line| !line.is_empty()).map(str::to_string).collect())
    }

    /// Commit what [`apply`] staged, as you, with your hooks. Anything else you had
    /// staged would go in too, so this is only called right after a clean apply.
    ///
    /// It is your commit, so it uses your git identity. If you have none set, git
    /// refuses, and the reason is passed back for the window to show.
    pub async fn commit(&self, message: &str) -> Result<(), String> {
        git(&self.project_dir, &["commit", "-q", "-m", message]).await.map(|_| ()).map_err(why)
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

/// The line of a git error worth showing. Git explains itself at length (the
/// "*** Please tell me who you are" speech is seven lines); the `fatal:` line at
/// the end is the part that says what actually went wrong.
fn why(problem: String) -> String {
    match problem.lines().rev().find(|line| line.starts_with("fatal:") || line.starts_with("error:")) {
        Some(line) => line.trim_start_matches("fatal:").trim_start_matches("error:").trim().to_string(),
        None => problem,
    }
}

/// The one sentence both dirty-folder checks use. `porcelain` is `git status --porcelain`.
///
/// The window watches for this wording and offers to commit the files itself, so the
/// user never has to leave the app for it. Keep the phrase "uncommitted changes".
fn dirty_message(porcelain: &str) -> String {
    let files = porcelain.lines().filter(|line| !line.trim().is_empty()).count();
    format!(
        "Your project folder has uncommitted changes ({}). The round cannot be put on top of them, \
         because applying writes over the files it touches.",
        crate::plural(files, "file")
    )
}

/// Commit everything in the project folder, including untracked files.
///
/// This is the app doing what it would otherwise tell the user to go and do in a
/// terminal. It is their commit, with their git identity and their hooks, so if they
/// have no identity set git refuses and the reason is passed back for the window.
pub async fn commit_project(project_dir: &Path, message: &str) -> Result<usize, String> {
    let porcelain = git(project_dir, &["status", "--porcelain"]).await?;
    let files = porcelain.lines().filter(|line| !line.trim().is_empty()).count();
    if files == 0 {
        return Ok(0);
    }
    git(project_dir, &["add", "-A"]).await.map_err(why)?;
    git(project_dir, &["commit", "-q", "-m", message]).await.map_err(why)?;
    Ok(files)
}

/// What is uncommitted in the project folder right now, if anything.
pub async fn uncommitted(project_dir: &Path) -> Option<String> {
    let porcelain = git(project_dir, &["status", "--porcelain"]).await.ok()?;
    if porcelain.trim().is_empty() {
        return None;
    }
    Some(dirty_message(&porcelain))
}

/// Delete this app's leftover branches that hold nothing.
///
/// Every round leaves a branch per worker plus one for the review. They are
/// deleted with the round, but an app that was force-quit never gets to do that,
/// and the project's branch list fills up with dead rounds. Two kinds are left
/// alone: a branch whose worktree is still there, because that round can still
/// be picked up, and a branch with work on it, because once the worktrees are
/// gone the branch is the only copy of it left. `git branch -d` is the safe
/// delete: it refuses a branch that is not already in the project's history,
/// which is exactly the branches worth keeping.
async fn clear_dead_branches(project_dir: &Path) {
    let Ok(listed) = git(project_dir, &["worktree", "list", "--porcelain"]).await else { return };
    let in_use: Vec<&str> =
        listed.lines().filter_map(|line| line.strip_prefix("branch refs/heads/")).collect();

    let ours = ["branch", "--list", "--format=%(refname:short)", "agent-console/*"];
    let Ok(branches) = git(project_dir, &ours).await else { return };
    for branch in branches.lines().map(str::trim).filter(|branch| !branch.is_empty()) {
        if !in_use.contains(&branch) {
            // -d, never -D: a branch git refuses to delete still has work on it.
            let _ = git(project_dir, &["branch", "-d", branch]).await;
        }
    }
}

/// Split one `git diff` into (path, patch) pairs, one per file.
fn split_patches(diff: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    for chunk in diff.split("\ndiff --git ") {
        let chunk = chunk.strip_prefix("diff --git ").unwrap_or(chunk);
        if chunk.trim().is_empty() {
            continue;
        }
        let header = chunk.lines().next().unwrap_or_default();
        // "a/src/main.rs b/src/main.rs" -> "src/main.rs". A renamed file keeps its
        // new name, which is the one the project will have.
        let Some(path) = header.rsplit(" b/").next().map(str::to_string) else { continue };
        files.push((path, format!("diff --git {chunk}")));
    }
    files
}

/// Lines added and removed in one file's patch.
fn count_changed_lines(patch: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

fn cut_to(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n... (this file's changes are too long to show in full)")
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
        // The identity "apply and commit" uses is the project's own, so the test
        // project needs one; the machine running the test may have none.
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
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

        assert_eq!(workspace.merge_into_review(&[1, 2]).await.unwrap(), Merge::Clean);
        let Changes::Diff(diff) = workspace.review_changes(None).await else { panic!("expected a diff") };
        assert!(diff.contains("+x = 2") && diff.contains("+z = 3"));

        // A second review only sees what changed after the first one.
        let reviewed = workspace.review_commit().await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 3\n").unwrap();
        workspace.commit_worker(1, "fix a", &files(&["a.py"])).await.unwrap();
        assert_eq!(workspace.merge_into_review(&[1, 2]).await.unwrap(), Merge::Clean);
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
        assert_eq!(workspace.merge_into_review(&[1]).await.unwrap(), Merge::Clean);
        assert_eq!(workspace.review_changes(None).await, Changes::Nothing);
        workspace.remove().await;
    }

    /// Two workers edit the same line of a.py, so git cannot merge them.
    async fn clashing_round() -> (tempfile::TempDir, Workspace) {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 2).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 'one'\n").unwrap();
        fs::write(workspace.worker_dir(2).join("a.py"), "x = 'two'\n").unwrap();
        workspace.commit_worker(1, "t1", &files(&["a.py"])).await.unwrap();
        workspace.commit_worker(2, "t2", &files(&["a.py"])).await.unwrap();
        (project, workspace)
    }

    #[tokio::test]
    async fn a_clash_leaves_the_merge_open_for_the_resolver() {
        let (_project, workspace) = clashing_round().await;
        let outcome = workspace.merge_into_review(&[1, 2]).await.unwrap();
        assert_eq!(outcome, Merge::Conflicted { worker: 2, files: files(&["a.py"]) });

        // The file has both sides in it, and the app can tell.
        let both = fs::read_to_string(workspace.review_dir().join("a.py")).unwrap();
        assert!(both.contains("<<<<<<<") && both.contains("x = 'one'") && both.contains("x = 'two'"));
        assert_eq!(workspace.still_marked(&files(&["a.py"])).await, files(&["a.py"]));
        // Staging a file that still has markers does not count as resolved.
        assert!(workspace.finish_merge(2, &files(&["a.py"]), "test").await.is_err());

        // What the resolver does: write a version with both sides in it.
        fs::write(workspace.review_dir().join("a.py"), "x = 'one and two'\n").unwrap();
        assert!(workspace.still_marked(&files(&["a.py"])).await.is_empty());
        workspace.finish_merge(2, &files(&["a.py"]), "resolved by the resolver").await.unwrap();
        assert_eq!(workspace.merge_into_review(&[1, 2]).await.unwrap(), Merge::Clean);

        let applied = workspace.apply().await.unwrap();
        assert_eq!(applied, files(&["a.py"]));
        workspace.remove().await;
    }

    #[tokio::test]
    async fn branches_from_a_force_quit_round_are_cleared_away_and_live_ones_kept() {
        let project = project();
        let abandoned = Workspace::create(project.path(), &run_id(), 1, 2).await.unwrap();
        let kept = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        // As a force-quit leaves it: the folders gone, the branches still there.
        fs::remove_dir_all(abandoned.root()).unwrap();

        let fresh = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        let left =
            run_git(project.path(), &["branch", "--list", "--format=%(refname:short)", "agent-console/*"]);
        let names: Vec<&str> = left.lines().collect();
        assert!(
            !names.iter().any(|branch| branch.starts_with(abandoned.branch_prefix())),
            "the dead round's branches are gone: {left}"
        );
        assert!(
            names.iter().any(|branch| branch.starts_with(kept.branch_prefix())),
            "a live round is untouched"
        );
        assert!(names.iter().any(|branch| branch.starts_with(fresh.branch_prefix())));

        kept.remove().await;
        fresh.remove().await;
    }

    #[tokio::test]
    async fn a_leftover_branch_that_still_holds_work_is_not_deleted() {
        let project = project();
        // A round that was force-quit after a worker had committed something.
        let abandoned = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        fs::write(abandoned.worker_dir(1).join("a.py"), "x = 2\n").unwrap();
        abandoned.commit_worker(1, "t", &files(&["a.py"])).await.unwrap();
        fs::remove_dir_all(abandoned.root()).unwrap();

        let fresh = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        let left =
            run_git(project.path(), &["branch", "--list", "--format=%(refname:short)", "agent-console/*"]);
        let worker = abandoned.branch("worker-1");
        assert!(
            left.lines().any(|branch| branch == worker),
            "the work is the only copy left, so the branch stays: {left}"
        );
        // The review branch of the same round had nothing on it, so it is gone.
        assert!(!left.lines().any(|branch| branch == abandoned.review_branch()), "{left}");

        fresh.remove().await;
        run_git(project.path(), &["branch", "-D", &worker]);
    }

    #[tokio::test]
    async fn a_round_can_be_opened_again_until_its_folders_are_gone() {
        let project = project();
        let run = run_id();
        let workspace = Workspace::create(project.path(), &run, 1, 2).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 2\n").unwrap();
        workspace.commit_worker(1, "t", &files(&["a.py"])).await.unwrap();

        // What the app saves about a round is enough to carry on with it.
        let again = Workspace::reopen(project.path(), &run, 1, workspace.worker_count(), workspace.base())
            .await
            .unwrap();
        assert_eq!(again.merge_into_review(&[1]).await.unwrap(), Merge::Clean);
        assert_eq!(again.apply().await.unwrap(), files(&["a.py"]));

        workspace.remove().await;
        let problem = Workspace::reopen(project.path(), &run, 1, 2, workspace.base()).await.unwrap_err();
        assert!(problem.contains("no longer there"), "{problem}");
    }

    #[tokio::test]
    async fn a_clash_can_be_abandoned_instead() {
        let (_project, workspace) = clashing_round().await;
        workspace.merge_into_review(&[1, 2]).await.unwrap();
        workspace.abort_merge().await;
        // Back to worker 1's version, with nothing half-merged.
        assert_eq!(fs::read_to_string(workspace.review_dir().join("a.py")).unwrap(), "x = 'one'\n");
        assert!(workspace.unresolved().await.is_empty());
        workspace.remove().await;
    }

    #[tokio::test]
    async fn the_changes_are_listed_one_file_at_a_time() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 2).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 1\ny = 2\n").unwrap();
        fs::write(workspace.worker_dir(2).join("c.py"), "z = 3\n").unwrap();
        workspace.commit_worker(1, "t1", &files(&["a.py"])).await.unwrap();
        workspace.commit_worker(2, "t2", &files(&["c.py"])).await.unwrap();
        workspace.merge_into_review(&[1, 2]).await.unwrap();

        let changes = workspace.file_changes().await.unwrap();
        let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
        assert_eq!(paths, ["a.py", "c.py"]);

        let first = &changes[0];
        assert_eq!(first.workers, vec![1], "each file says which worker changed it");
        assert_eq!((first.added, first.removed), (1, 0), "one line added, the other unchanged");
        assert!(first.patch.contains("+y = 2") && !first.patch.contains("z = 3"));
        assert_eq!(changes[1].workers, vec![2]);
        workspace.remove().await;
    }

    #[test]
    fn a_git_error_is_cut_down_to_the_line_that_says_what_went_wrong() {
        let speech = "Author identity unknown\n\n*** Please tell me who you are.\n\nRun\n\n  git \
                      config --global user.email \"you@example.com\"\n\nfatal: unable to auto-detect \
                      email address (got 'runner@fv-az123.(none)')";
        assert_eq!(
            why(speech.to_string()),
            "unable to auto-detect email address (got 'runner@fv-az123.(none)')"
        );
        // Nothing to cut down to: the whole thing is kept rather than lost.
        assert_eq!(why("something odd".to_string()), "something odd");
    }

    #[tokio::test]
    async fn applying_can_commit_too() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 2\n").unwrap();
        workspace.commit_worker(1, "t", &files(&["a.py"])).await.unwrap();
        workspace.merge_into_review(&[1]).await.unwrap();
        workspace.apply().await.unwrap();

        workspace.commit("add the thing").await.unwrap();
        let log = run_git(project.path(), &["log", "--oneline"]);
        assert_eq!(log.lines().count(), 2);
        assert!(log.contains("add the thing"), "{log}");
        assert!(run_git(project.path(), &["status", "--porcelain"]).trim().is_empty(), "nothing left over");
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
        assert!(problem.contains("uncommitted changes (1 file)"), "{problem}");
    }

    #[tokio::test]
    async fn apply_waits_for_the_folder_to_be_tidy_but_not_for_it_to_stand_still() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "x = 2\n").unwrap();
        workspace.commit_worker(1, "t", &files(&["a.py"])).await.unwrap();
        assert_eq!(workspace.merge_into_review(&[1]).await.unwrap(), Merge::Clean);

        // Something uncommitted would be written over, so applying waits.
        fs::write(project.path().join("b.py"), "y = 2\n").unwrap();
        let problem = workspace.apply().await.unwrap_err();
        assert!(problem.contains("uncommitted changes (1 file)"), "{problem}");

        // Committed, and in another file: the round goes on top of it.
        run_git(project.path(), &["add", "-A"]);
        run_git(project.path(), &["commit", "-qm", "my own work"]);
        let applied = workspace.apply().await.unwrap();
        assert_eq!(applied, ["a.py"]);
        assert_eq!(fs::read_to_string(project.path().join("b.py")).unwrap(), "y = 2\n");
        workspace.remove().await;
    }

    #[tokio::test]
    async fn a_clash_with_the_project_s_own_new_commits_leaves_the_project_untouched() {
        let project = project();
        let workspace = Workspace::create(project.path(), &run_id(), 1, 1).await.unwrap();
        fs::write(workspace.worker_dir(1).join("a.py"), "the worker's line\n").unwrap();
        workspace.commit_worker(1, "t", &files(&["a.py"])).await.unwrap();
        assert_eq!(workspace.merge_into_review(&[1]).await.unwrap(), Merge::Clean);

        // The same file, the same line, committed in the project meanwhile.
        fs::write(project.path().join("a.py"), "my own line\n").unwrap();
        run_git(project.path(), &["commit", "-qam", "my own work"]);

        let problem = workspace.apply().await.unwrap_err();
        assert!(problem.contains("could not be put together"), "{problem}");
        assert!(problem.contains("untouched"), "{problem}");
        // Nothing half-merged left behind, and my own line is still mine.
        assert_eq!(fs::read_to_string(project.path().join("a.py")).unwrap(), "my own line\n");
        assert!(run_git(project.path(), &["status", "--porcelain"]).trim().is_empty());
        workspace.remove().await;
    }

    #[tokio::test]
    async fn the_app_can_commit_the_folder_itself_so_nobody_needs_a_terminal() {
        let project = project();
        fs::write(project.path().join("a.py"), "changed\n").unwrap();
        fs::write(project.path().join("brand-new.py"), "new\n").unwrap();

        assert!(uncommitted(project.path()).await.is_some());
        let committed = commit_project(project.path(), "wip").await.unwrap();
        assert_eq!(committed, 2, "the untracked file counts too");

        assert!(uncommitted(project.path()).await.is_none());
        assert_eq!(commit_project(project.path(), "wip").await.unwrap(), 0, "nothing left to commit");
    }
}
