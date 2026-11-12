//! Run only the planner from the terminal and print every event as JSON.
//!
//! Handy for checking that `claude` is found and signed in, without the app:
//!
//! ```sh
//! cargo run -p agent-console-core --example plan -- ~/projects/some-repo "Add a --version flag"
//! ```

use std::sync::Arc;

use agent_console_core::claude::{locate, ClaudeRunner};
use agent_console_core::orchestrator::supervised;
use agent_console_core::run::EventSink;
use agent_console_core::settings::{self, Settings};
use agent_console_core::{config, Event, Orchestrator, Run};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(project_dir), Some(request)) = (args.next(), args.next()) else {
        eprintln!("usage: plan <project-folder> <request>");
        std::process::exit(2);
    };

    let location = match locate::locate() {
        Ok(location) => location,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    eprintln!("using {}", location.program.display());

    let print: EventSink =
        Arc::new(|_run_id: &str, event: &Event| println!("{}", serde_json::to_string(event).unwrap()));
    let run = Run::new(project_dir, request, print);
    let runner = Arc::new(ClaudeRunner::new(location, config::AUTH));
    let orchestrator = Orchestrator::new(runner, settings::shared(Settings::default()));
    supervised(run.clone(), orchestrator.plan(&run, None)).await;
}
