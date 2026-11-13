//! The engine of Agent Console, with no UI code in it.
//!
//! Reading order if you are new to the codebase:
//!
//! 1. [`config`]: who the agents are and what they may do; [`settings`]: what
//!    the user can change in the Settings window (models, effort, budget).
//! 2. [`orchestrator`]: the order they run in (plan, build, review).
//! 3. [`runner`]: how one agent is started and reported on.
//! 4. [`claude`]: the only code that talks to the `claude` program;
//!    [`changes`]: the worker's diff that goes into the reviewer's prompt;
//!    [`worktrees`]: each worker's own copy of the project, and Apply/Discard.
//! 5. [`run`] and [`event`]: the state of one job and what the UI is told.
//!
//! The Tauri app in `src-tauri/` is a thin layer over this crate. Keeping the
//! engine separate means it can be tested without opening a window.

pub mod changes;
pub mod checks;
pub mod claude;
pub mod config;
pub mod event;
pub mod orchestrator;
pub mod process;
pub mod project;
pub mod run;
pub mod runner;
pub mod settings;
pub mod worktrees;

/// "1 file", "3 files": a count with its word, so no message says "1 file(s)".
pub fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

pub use event::{Event, EventKind};
pub use orchestrator::Orchestrator;
pub use run::{Run, RunState};
pub use runner::{AgentError, AgentOutput, AgentRunner};
pub use settings::{Settings, SharedSettings};

#[cfg(test)]
mod tests {
    use super::plural;

    #[test]
    fn counts_come_with_the_right_word() {
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
    }
}
