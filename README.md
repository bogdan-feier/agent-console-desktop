# Agent Console

A macOS desktop app that runs a small team of Claude agents on one project.

```text
you -> dispatcher -> answers quick questions itself, or passes your message to the planner
       planner -> talks ideas through with you, then makes a plan -> you approve
       workers -> up to 3 at the same time, each in its own copy of the project
       reviewer -> checks all their work together -> you apply it, or discard it
```

| Agent | Default model | What it does | What it may do |
|---|---|---|---|
| Dispatcher | Haiku | The front desk: answers quick questions, routes everything else | Read files only |
| Planner | Opus | Brainstorms with you, then splits the work into up to three tasks | Read files only |
| Workers 1 to 3 | Sonnet | Each builds one task, in its own git worktree | Read and edit files, run tests and linters |
| Reviewer | Sonnet | Checks all the workers' changes merged together | Read files, run tests, linters and `git diff` |

Built with Rust and Tauri 2. The page is plain HTML, CSS and JavaScript shown in the
system's own web view. Each agent is a separate `claude` session, so the target project's
`CLAUDE.md`, hooks and `.claude/settings.json` apply to every agent, just as they do in the
terminal. Skills and MCP servers are left out to save usage (see "Keeping usage down").
Powered by Claude.

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
| `cargo test --workspace` | Run all tests. They use a fake `claude`, so they never call the real one. |
| `cargo fmt --all` | Format the Rust code. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Run the linter; warnings count as errors. |
| `cargo tauri build` | Build `Agent Console.app` into `target/release/bundle/macos/`. |

To let Claude Code run the test and lint commands in this repo without asking each time:

```bash
mkdir -p .claude && cp docs/claude-settings.example.json .claude/settings.json
```

## How a round goes

The project must be a git repository. When you approve a plan, it must have no uncommitted
changes (see "Why worktrees" below).

The window has two halves. On the left is the conversation: you, the dispatcher and the
planner, plus the cards where you approve a plan or act on a review. On the right is the
team: a card for each worker and one for the reviewer. The strip at the top shows which
step of the round you are at.

1. **Message the team.** Choose the project in the top left, type in the box at the bottom of
   the conversation and press **Send** (or Cmd+Enter). With **Ask** selected, the dispatcher
   answers quick questions itself ("what does this function do?"). Ideas go to the planner to
   talk through; clear requests go to the planner to plan. Select **Brainstorm** or **Plan** to
   skip the dispatcher.
2. **Brainstorm (optional).** Keep talking to the planner with **Brainstorm** selected.
   Nothing is edited.
3. **Plan.** Select **Plan** and send (typing something is optional). The planner turns the
   conversation into one to three tasks. Edit the tasks in the plan card directly, or send
   **Plan** again with what to change.
4. **Approve.** This is where work starts. Each worker gets its own copy of the project (a
   git worktree) and builds its task there. Your project folder is not touched.
5. **Review.** When the workers finish, their work is merged together and the reviewer
   checks all of it at once, running the tests on the combined result. Each problem it finds
   names the task it belongs to.
6. **Fix (optional).** "Send back to Worker N" gives that worker the problems in its task;
   the reviewer then checks again, looking only at what changed. You can also message any
   worker or the reviewer directly, then press **Review again**.
7. **Apply or discard.** **Apply to project** puts the result into your project as staged
   changes: look at them with `git diff --staged` and commit them yourself. **Discard**
   throws the round away. Either way the worktrees are deleted, and you can start the next
   round in the same run: the dispatcher and the planner remember the conversation.

**Stop** ends whatever is running and kills the `claude` processes it started. **New run**
starts over with fresh agents (apply or discard the open round first).

## Why worktrees

Three workers editing one folder at the same time can overwrite each other. Instead, each
worker gets its own git worktree: a separate folder attached to your repository, on its
own branch (`agent-console/<run>-round<N>/worker-<N>`). The worktrees live in the system's
temporary folder, not in your project.

- **Clean start.** Workers start from your last commit, which is why uncommitted changes
  must be committed or stashed before you approve a plan.
- **Clashes.** The planner gives each task different files. If two workers still change
  the same file, the merge before the review fails, and the reviewer's card says which
  file. The app also warns when a worker changes a file outside its task.
- **Apply needs a clean, unchanged project.** If you committed something in the meantime,
  Apply tells you the branch to merge yourself (`git merge --squash <branch>`).
- **Slower first test run.** Each worktree is a fresh checkout without ignored folders
  such as `.venv`, `node_modules` or `target`, so the first test run in it sets those up again.
- **Quitting the app** with a round still open leaves its branches in your repository, so
  nothing is lost. To clean them up later:
  `git worktree prune && git branch --list 'agent-console/*' | xargs git branch -D`.

## Settings

**Start from** fills everything in with one of three presets:

| Preset | Dispatcher | Planner | Workers | Reviewer | Cap per session |
|---|---|---|---|---|---|
| Lean | Haiku | Sonnet, medium effort | Sonnet, low | Sonnet, low | 3 dollars |
| Balanced (the default) | Haiku | Opus, medium | Sonnet, medium | Sonnet, medium | 5 dollars |
| Thorough | Haiku | Opus, high | Sonnet, high | Opus, medium | 10 dollars |

You can then change any of them. For each role you set:

- **Model:** Opus, Sonnet, Haiku, or any model ID you type under "Other".
- **Effort:** how hard the model thinks before answering (Low to Max). "Default" leaves it to
  Claude Code. Higher effort is slower and uses more of your limits.

It also sets the spending cap per agent session, and whether agents get your MCP servers
(off by default). Changes apply to the next agent that
starts, even in the middle of a run. They are saved in
`~/Library/Application Support/dev.agentconsole.desktop/settings.json`. The starting
defaults are the constants at the top of `crates/core/src/config.rs`.

## Signing in

By default the agents use the account you are logged in to Claude Code with. Usage counts
against that plan's limits, the same as using Claude Code in a terminal, and one round can
be six sessions or more. An `ANTHROPIC_API_KEY` in your environment is ignored on purpose so you
are never billed on the API by accident.

To pay per use with an API key instead, set `AUTH` to `AuthMode::ApiKey` in
`crates/core/src/config.rs` and start the app from a terminal where the key is set.

This sign-in is for running the app yourself. A tool you hand to other people has to use
API keys.

## Keeping usage down

Every agent is a full Claude Code session, and every turn of a session sends the whole
conversation so far back to the model. Most of what this app does to save usage follows from
that.

**What the app does for you**

- **Leaner sessions.** Agents start without skill descriptions and without your MCP servers,
  which would otherwise be sent on every turn of every agent. The agents only need file and
  shell tools. (These options are only passed if your version of Claude Code lists them in
  `claude --help`, so an older version still works, just without this saving.)
- **Dispatcher first.** Quick questions are answered by Haiku, and the Opus planner only runs
  when it is needed.
- **Shared prompt cache.** The parts of the system prompt that change between sessions are
  moved out of it, and parallel workers start 5 seconds apart. Each can then read the
  previous one's cached prompt, which costs far less than sending it fresh.
- **One task when one is enough.** The planner is told to split work only when the parts are
  big and independent. Each extra task means another worker.
- **One reviewer for everything.** One session checks all the tasks together and runs the
  tests once, instead of one reviewer per worker. Its prompt includes the merged diff, so
  it does not spend turns finding and reading files. A second review continues the same
  session and only gets what changed since the first.
- **Economical instructions.** Every agent is told to search before reading, read only the
  parts of files it needs, and not re-read what it has already seen.
- **Limits.** Each session has a turn limit (`*_MAX_TURNS` in `config.rs`) and the spending
  cap from Settings. An agent that stops at a limit keeps its session: send it a message and
  it continues where it stopped.
- **Explicit effort.** The presets set an effort level for every role instead of leaving it to
  Claude Code's default, so you know what you are getting.

**What you can do**

- **Watch the numbers.** Each agent's reply ends with the tokens its session used, and the top
  bar shows the run total. "From cache" is the cheap kind.
- **Try the Lean preset** for routine work, and Thorough only when quality matters most.
- **Keep brainstorms short.** Each message resends the conversation so far (mostly from
  cache, but it grows). When you know what you want, send Plan.
- **Plan straight away** for small, clear changes: it skips the dispatcher and the brainstorm.
- **Keep the target project's `CLAUDE.md` short.** Every agent reads it at the start.
- **Ask for one thing per round.** Two unrelated requests in one round cost more than two
  small rounds, because every agent carries both in its context.
- **Start a new run now and then.** The dispatcher and the planner keep their conversation across
  rounds, which helps, but it also keeps growing. **New run** starts them fresh.

## Changing things

| To change | Edit |
|---|---|
| Default models and cap, turn limits, allowed shell commands, role instructions | `crates/core/src/config.rs` |
| What the Settings window can change, the presets, and how it is saved | `crates/core/src/settings.rs` |
| What goes into the reviewer's prompt about the changes | `crates/core/src/changes.rs` |
| The order agents run in | `crates/core/src/orchestrator.rs` |
| How worktrees are created, merged, applied and removed | `crates/core/src/worktrees.rs` |
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

- **Check the file lists before you approve.** Tasks that share a file will clash when
  their work is merged.
- **Shell commands are limited.** Agents may run the commands in `CHECK_COMMANDS`
  (`config.rs`) plus whatever the project's `.claude/settings.json` allows. Everything
  else is refused and shows up in that agent's steps as "Refused or failed".
- **Each session has a cap.** A session stops at the spending cap in Settings (5 dollars of
  API-equivalent usage by default). Long conversations with one agent count toward the same
  session, so a long brainstorm can reach it.
- **Runs live in memory.** Quitting the app forgets them (an open round's branches stay in
  your repository; see "Why worktrees").
- **No official SDK underneath.** There is no Agent SDK for Rust, so this app reads the
  `claude` CLI's JSON output itself (`crates/core/src/claude/protocol.rs`). If a Claude
  Code update changes that output, this is the file to fix.

## Troubleshooting

| What you see | What to do |
|---|---|
| "The project has uncommitted changes" when approving | Commit or stash them, then approve again. The plan is still there. |
| "Worker N's changes clash with another worker's" | Two tasks changed the same file. Message one worker to undo its change there and press Review again, or discard and plan again. |
| "Your project has new commits since the workers started" | Run the `git merge --squash` command it shows, then Discard to clean up. |
| An agent fails right after you change its effort | Some models do not support every effort level. Set it back to Default. |
| "Stopped at this agent's turn limit" | Send the agent a message such as "continue" to let it finish, or raise its limit in `config.rs`. |
| "Could not find the `claude` program" | Install Claude Code, or start the app with `AGENT_CONSOLE_CLAUDE=/full/path/to/claude`. |
| "Claude could not sign in" in the conversation | Run `claude` in a terminal, type `/login`, finish in the browser, try again. |
| A tool line says "Refused or failed" | The command is not on the allow list. Add it to `CHECK_COMMANDS` or to the project's `.claude/settings.json`. |
| Want to see the raw events without the app | `cargo run -p agent-console-core --example plan -- <folder> "<request>"` |
