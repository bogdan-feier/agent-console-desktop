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
| `cargo tauri dev` | Run the app from source. Changes to Rust or to `ui/` reload it. |
| `AGENT_CONSOLE_DEMO=1 cargo tauri dev` | The same, with demo mode on: scripted agents, nothing edited, nothing spent. |
| `cargo test --workspace` | Run all tests. They use a fake `claude`, so they never call the real one. |
| `cargo fmt --all` | Format the Rust code. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Run the linter; warnings count as errors. |
| `cargo tauri build` | Build `Agent Console.app` into `target/release/bundle/macos/`. |

To let Claude Code run the test and lint commands in this repo without asking each time:

```bash
mkdir -p .claude && cp docs/claude-settings.example.json .claude/settings.json
```

## How a run goes

1. **Brainstorm (optional).** Type what you want and press **Brainstorm**. The planner reads
   the project and discusses it with you: options, trade-offs, questions. Keep talking in
   the box at the bottom of the planner's panel (Cmd+Enter sends). Nothing is edited.
   Press **Plan right away** instead to skip the discussion.
2. **Make plan.** When you are ready, press **Make plan**. The planner turns the
   conversation into up to two tasks. Anything typed in the box is passed along as extra
   direction ("keep it in one file").
3. **Adjust or approve.** Edit the tasks directly, or tell the planner what to change and
   press Make plan again. Then approve. This is the one point where work starts.
4. **Build.** The workers run in parallel, each on its own task and its own files.
5. **Review.** Each reviewer checks one worker and answers "Approved" or "Needs changes".
6. **Follow up.** Every agent that has worked has a message box. Ask a worker for a change,
   ask a reviewer why it decided something, or press "Send back to Worker N" to hand it the
   review. Each agent continues its own session, so it remembers what it did and said.
   You can also make a new plan for a next round.

"Stop" ends whatever is running and kills the `claude` processes it started.

## Settings

The **Settings** button sets, for the planner, the workers and the reviewers:

- **Model:** Opus, Sonnet, Haiku, or any model ID you type under "Other".
- **Effort:** how hard the model thinks before answering (Low to Max). "Default" leaves it to
  Claude Code. Higher effort is slower and uses more of your limits.

It also sets the spending cap per agent session. Changes apply to the next agent that
starts, even in the middle of a run. They are saved in
`~/Library/Application Support/dev.agentconsole.desktop/settings.json`. The starting
defaults are the constants at the top of `crates/core/src/config.rs`.

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
| Default models and cap, allowed shell commands, role instructions | `crates/core/src/config.rs` |
| What the Settings window can change, and how it is saved | `crates/core/src/settings.rs` |
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
- **Each session has a cap.** A session stops at the spending cap in Settings (5 dollars of
  API-equivalent usage by default). Long conversations with one agent count toward the same
  session, so a long brainstorm can reach it.
- **Runs live in memory.** Quitting the app forgets them. The edits the agents made stay
  on disk.
- **No official SDK underneath.** There is no Agent SDK for Rust, so this app reads the
  `claude` CLI's JSON output itself (`crates/core/src/claude/protocol.rs`). If a Claude
  Code update changes that output, this is the file to fix.

## Troubleshooting

| What you see | What to do |
|---|---|
| An agent fails right after you change its effort | Some models do not support every effort level. Set it back to Default. |
| "Could not find the `claude` program" | Install Claude Code, or start the app with `AGENT_CONSOLE_CLAUDE=/full/path/to/claude`. |
| "Claude could not sign in" in the planner panel | Run `claude` in a terminal, type `/login`, finish in the browser, try again. |
| A tool line says "Refused or failed" | The command is not on the allow list. Add it to `CHECK_COMMANDS` or to the project's `.claude/settings.json`. |
| Want to see the raw events without the app | `cargo run -p agent-console-core --example plan -- <folder> "<request>"` |
