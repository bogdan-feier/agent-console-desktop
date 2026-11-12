# Agent Console

A macOS desktop app that runs a small team of Claude agents on one of your projects.

| Agent | Model | What it does | What it may do |
|---|---|---|---|
| Planner | Opus | Reads the project and splits your request into up to two tasks | Read files only |
| Worker 1, Worker 2 | Sonnet | Each implements one task, at the same time | Read and edit files, run tests and linters |
| Reviewer 1, Reviewer 2 | Sonnet | Each checks one worker's result and gives a verdict | Read files, run tests, linters and `git diff` |

Built with Rust and Tauri 2. The page is plain HTML, CSS and JavaScript shown in the
system's own web view. Each agent is a separate `claude` session, so the target project's
`CLAUDE.md`, skills, hooks and `.claude/settings.json` apply to every agent, just as they
do in the terminal. Powered by Claude.

## First-time setup

You need Claude Code installed and logged in (`claude`, then `/login`). Then:

```bash
cd ~/agent-console-desktop
bash scripts/setup.sh
```

The script installs what is missing (Xcode command line tools, Rust, the Tauri command
line tool), runs the tests and builds the app. The first run takes 10 to 15 minutes,
almost all of it compiling. When it finishes:

```bash
open "target/release/bundle/macos/Agent Console.app"
```

## Everyday commands

| Command | What it does |
|---|---|
| `make dev` | Run the app from source. Rust changes rebuild and restart it. |
| `make demo` | The same, with demo mode on: scripted agents, nothing edited, nothing spent. |
| `make test` | Run all tests. They use a fake `claude`, so they never call the real one. |
| `make lint` | Check formatting and run clippy. |
| `make build` | Build `Agent Console.app`. |

Changes to files in `ui/` show up after reloading the window (Cmd+R) in `make dev`.

## How a run goes

1. **Plan.** The planner explores the project and proposes tasks. Nothing is edited.
2. **You approve.** Edit the task titles, instructions or file lists if you like, then
   approve. This is the one point where a person decides.
3. **Build.** The workers run in parallel, each on its own task and its own files.
4. **Review.** Each reviewer checks one worker and answers "Approved" or "Needs changes".
5. **Fix (optional).** "Send back to Worker N" gives the review to the same worker, which
   still remembers its task, and then reviews it again.

"Stop" ends the current phase and kills the `claude` processes it started.

## Signing in

By default the agents use the account you are logged in to Claude Code with. Usage counts
against that plan's limits, the same as using Claude Code in a terminal, and one run here
is five sessions. An `ANTHROPIC_API_KEY` in your environment is ignored on purpose so you
are never billed on the API by accident.

To pay per use with an API key instead, set `AUTH` to `AuthMode::ApiKey` in
`crates/core/src/config.rs` and start the app from a terminal where the key is set.

This sign-in is for running the app yourself. A tool you hand to other people has to use
API keys.

## Changing things

| To change | Edit |
|---|---|
| Models, the spending cap, allowed shell commands, role instructions | `crates/core/src/config.rs` |
| The order agents run in | `crates/core/src/orchestrator.rs` |
| How `claude` is started, or how its output is read | `crates/core/src/claude/` |
| What the page looks like | `ui/styles.css`, `ui/index.html` |
| What the page does | `ui/app.js` |
| Window size, app name, icon | `src-tauri/tauri.conf.json`, `assets/app-icon.png` |

`docs/ARCHITECTURE.md` explains how the parts fit together, with notes for someone coming
from Python.

To regenerate the icon set after editing `assets/app-icon.png`:

```bash
cargo tauri icon assets/app-icon.png -o src-tauri/icons
```

## Things to know before pointing it at real code

- **Workers share one folder.** They run at the same time in the same working tree. The
  planner is told to give each task different files, and each worker is told to stay in
  its list, but nothing enforces that. Check the file lists before you approve.
- **Commit first.** Start from a clean `git status` so you can see, and undo, exactly
  what the agents changed.
- **Shell commands are limited.** Agents may run the commands in `CHECK_COMMANDS`
  (`config.rs`) plus whatever the project's `.claude/settings.json` allows. Everything
  else is refused and shows up in the panel as "Refused or failed".
- **Each session has a cap.** A session stops at `MAX_BUDGET_USD_PER_AGENT` (5 dollars of
  API-equivalent usage by default).
- **Runs live in memory.** Quitting the app forgets them. The edits the agents made stay
  on disk.
- **No official SDK underneath.** There is no Agent SDK for Rust, so this app reads the
  `claude` CLI's JSON output itself (`crates/core/src/claude/protocol.rs`). If a Claude
  Code update changes that output, this is the file to fix.

## Troubleshooting

| What you see | What to do |
|---|---|
| "Could not find the `claude` program" | Install Claude Code, or start the app with `AGENT_CONSOLE_CLAUDE=/full/path/to/claude`. |
| "Claude could not sign in" in the planner panel | Run `claude` in a terminal, type `/login`, finish in the browser, try again. |
| A tool line says "Refused or failed" | The command is not on the allow list. Add it to `CHECK_COMMANDS` or to the project's `.claude/settings.json`. |
| Want to see the raw events without the app | `cargo run -p agent-console-core --example plan -- <folder> "<request>"` |
