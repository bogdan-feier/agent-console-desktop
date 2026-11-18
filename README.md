# Agent Console

A macOS desktop app that runs a small team of Claude agents on one project.

```text
you -> dispatcher -> answers quick questions itself, sends small clear fixes to one worker,
                     or passes your message to the planner
       planner -> talks ideas through with you, then makes a plan -> you approve
       workers -> up to 3 at the same time, each in its own copy of the project
       reviewer -> checks all their work together -> you apply it, or discard it
```

| Agent | Default model | What it does | What it may do |
|---|---|---|---|
| Dispatcher | Haiku | The front desk: answers quick questions, routes everything else | Read files only |
| Planner | Opus | Brainstorms with you, then splits the work into up to three tasks | Read files only |
| Workers 1 to 3 | Haiku | Each builds one task, in its own git worktree | Read and edit files, run tests and linters |
| Reviewer | Sonnet | Checks all the workers' changes merged together | Read files, run tests, linters and `git diff` |

Built with Rust and Tauri 2. The page is plain HTML, CSS and JavaScript shown in the
system's own web view. Each agent is a separate `claude` session, so the target project's
`CLAUDE.md`, hooks and `.claude/settings.json` apply to every agent, just as they do in the
terminal. The planner, workers and reviewer can also use the skills from your installed plugins; the
dispatcher and resolver leave skills out, and every agent leaves MCP servers out, to save usage (see "Keeping usage down").
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
planner, plus the cards where you approve a plan. On the right is the team: a card for
each worker, one for the reviewer (where its review appears, with a button to send each
problem back), and one for the resolver that only appears when it is needed. The strip at the top shows which step of the round you are at.

1. **Message the team.** Choose the project in the top left, type in the box at the bottom of
   the conversation and press **Send** (or Cmd+Enter). With **Ask** selected, the dispatcher
   answers quick questions itself ("what does this function do?"). Ideas go to the planner to
   talk through; clear requests go to the planner to plan. A small change you describe
   completely (a typo, a renamed option, a new default) goes straight to one worker as a
   quick change, with no plan and no review; when in doubt it plans instead. Select
   **Brainstorm**, **Plan** or **Quick** to skip the dispatcher.
2. **Brainstorm (optional).** Keep talking to the planner with **Brainstorm** selected.
   Nothing is edited.
3. **Plan.** Select **Plan** and send (typing something is optional). The planner turns the
   conversation into one to three tasks. Edit the tasks in the plan card directly, or send
   **Plan** again with what to change.
4. **Approve.** This is where work starts. Each worker gets its own copy of the project (a
   git worktree) and builds its task there. Your project folder is not touched.
5. **Review.** When the workers finish, their work is merged together and the reviewer
   checks all of it at once. Each problem it finds names the task it belongs to.
   Two things happen before it looks:
   - If two workers changed the same lines, git cannot merge them. The **resolver** then
     writes a version with both changes in it, touching only the clashing files. If it
     cannot, the merge is undone and you are told which files clashed; nothing is lost.
   - If you set a **check command** in Settings, the app runs it on the merged result and
     gives the output to the reviewer. That costs no tokens and saves the reviewer the turns.
     With **Skip the review when this command passes** on (off by default), a round whose
     checks pass ends there, and **Review again** still calls the reviewer in.
6. **Fix (optional).** "Send back to Worker N" gives that worker the problems in its task;
   the reviewer then checks again, looking only at what changed. The review itself is on
   the reviewer's card. You can also message any worker directly, then press **Review again**.
7. **Apply or discard.** **See changes and apply** opens the round's changes file by file,
   with the lines each worker added and removed. From there, **Apply only** puts them into
   your project as staged changes for you to commit, and **Apply and commit** also makes the
   commit, with the message in the box (the plan's summary, which you can edit). **Discard**
   throws the round away. Either way the worktrees are deleted, and you can start the next
   round in the same run: the dispatcher and the planner remember the conversation.

**Stop** ends whatever is running and kills the `claude` processes it started. **New run**
starts over with fresh agents (apply or discard the open round first).

Every run is saved as it happens. **Runs** in the top bar lists them all with what each one
used and how it was set up (the preset, or the models), so you can see whether Lean really is
cheaper for your kind of work. Opening one picks it up where it left off: the agents remember
the conversation, and an open round can still be reviewed, applied or discarded, even after
the app has been closed and reopened. A run opens read only when something it needs is gone
(the project folder has moved, or the workers' copies have been cleared out of the system's
temporary folder, which macOS does after a few days); it then says which.

Prompts you type often can be kept: the **Save this prompt** chip above the message box
keeps what you typed, and the chips put it back.

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

The Settings window has four sections. At the top, **Keep separate settings for this
project** decides what saving changes: with it off, the general settings every project
starts from; with it on, this project's own set, which overrides them.

**Models and limits.** **Start from** fills this section in with one of three presets
(the other sections stay as they are):

| Preset | Dispatcher | Planner | Workers | Reviewer | Cap per session |
|---|---|---|---|---|---|
| Lean | Haiku | Sonnet, medium effort | Haiku | Sonnet, low | 3 dollars |
| Balanced (the default) | Haiku | Opus, medium | Haiku | Sonnet, medium | 5 dollars |
| Thorough | Haiku | Opus, high | Sonnet, high | Opus, medium | 10 dollars |

You can then change any of them. For each role you set:

- **Model:** Opus, Sonnet, Haiku, or any model ID you type under "Other".
- **Effort:** how hard the model thinks before answering (Low to Max). "Default" leaves it to
  Claude Code. Higher effort is slower and uses more of your limits.
- **Turn limit:** the most replies one session may take before it stops. Empty means the
  default shown in grey (8 for the dispatcher, 30 for the planner and the reviewer, 60 for
  workers).

Below the roles: the turn limit when you message an agent directly (20 by default), and the
spending cap per agent session.

**Permissions.** Agents run with nobody there to click "allow", so anything not allowed is
refused. You choose:

- **Shell commands** the workers and the reviewer may run, one per line, written as you would
  type them (`npm test`). A `*` at the end matches the rest of the command. A lone `*`, which
  would allow everything, is refused. An empty list takes the shell away from them. Rules in
  the project's own `.claude/settings.json` still apply on top.
  **Suggest from this project** reads the project's own files (`Cargo.toml`, `package.json`
  and its scripts, `pyproject.toml`, `go.mod`, `Makefile`) and offers the commands that kind
  of project usually needs, plus a check command if there is an obvious one. They go into the
  boxes for you to look over; nothing is allowed until you save.
- **Web search** for the dispatcher and the planner (off by default). Only those two get it:
  they cannot edit files or run commands, so a web page with hidden instructions cannot make
  them change your project.
- **MCP servers** for every agent (off by default, because each server's tool list is sent on
  every turn).

Only workers can ever edit files, and only in their own copy of the project; that is not a
setting.

**Team and checks.** The most workers one plan may use, from 1 to 3; fewer means smaller
plans and less usage. Whether an agent should merge two workers' changes when they clash
(on by default). And a command the app runs itself on the merged changes before the reviewer
sees them, such as `cargo test`: its output goes to the reviewer, which costs nothing and
saves it the turns.

**Notifications.** When a round needs you (a plan to approve, a review to act on, an error)
and Agent Console is not the app in front, it sends a desktop notification. macOS shows its
permission box the first time one is sent, which is when your first run starts; **Send a test
one** brings it up on purpose and proves the rest works. If you miss or decline that box,
macOS never shows it again: turn notifications on under System Settings, Notifications,
Agent Console. They are unreliable from `cargo tauri dev`, where the app is a bare binary
rather than an installed one; test them with the app built by `cargo tauri build`.

**Notes for every agent.** Text added to every agent's instructions, for rules about the
project or how you like to work ("Use uv, never pip"). It is sent on every turn, mostly from
the cache, so keep it short (at most 4000 characters).

Changes apply to the next agent that starts, even in the middle of a run. They are saved in
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

- **Leaner sessions.** Agents start without your MCP servers, and the dispatcher and
  resolver start without skill descriptions, which would otherwise be sent on every turn.
  The planner, workers and reviewer keep them so they can plan, build and review by the
  conventions your installed plugins teach. To give them only some plugins, list their
  folders under "Plugin folders the agents load" in Settings: they then skip your user
  settings and load just those. (These options are only passed if your version of Claude Code lists them in
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
- **Limits.** Each session has a turn limit and the spending cap, both from Settings. An agent that stops at a limit keeps its session: send it a message and
  it continues where it stopped.
- **Explicit effort.** The presets set an effort level for every role instead of leaving it to
  Claude Code's default, so you know what you are getting.

**What you can do**

- **Watch the numbers.** Each agent's reply ends with the tokens its session used, and the top
  bar shows the run total. "From cache" is the cheap kind.
- **Try the Lean preset** for routine work, and Thorough only when quality matters most.
- **Keep brainstorms short.** Each message resends the conversation so far (mostly from
  cache, but it grows). When you know what you want, send Plan.
- **Quick for small, clear changes:** one worker and no review, about a quarter of a full
  round. Ask sends the obvious ones there by itself; choose Quick to make sure.
- **Keep the target project's `CLAUDE.md` short.** Every agent reads it at the start.
- **Ask for one thing per round.** Two unrelated requests in one round cost more than two
  small rounds, because every agent carries both in its context.
- **Start a new run now and then.** The dispatcher and the planner keep their conversation across
  rounds, which helps, but it also keeps growing. **New run** starts them fresh.

## Changing things

| To change | Edit |
|---|---|
| Default models and cap, default turn limits and shell commands, role instructions | `crates/core/src/config.rs` |
| What the Settings window can change, the presets, and how it is saved | `crates/core/src/settings.rs` |
| What goes into the reviewer's prompt about the changes | `crates/core/src/changes.rs` |
| The order agents run in | `crates/core/src/orchestrator.rs` |
| How worktrees are created, merged, applied and removed | `crates/core/src/worktrees.rs` |
| How `claude` is started, or how its output is read | `crates/core/src/claude/` |
| How clashes between workers are merged | `crates/core/src/worktrees.rs`, `resolve` in `orchestrator.rs` |
| How the project's own checks are run | `crates/core/src/checks.rs` |
| How runs are saved, listed and picked up again | `src-tauri/src/store.rs`, `Run::snapshot` and `Run::from_saved` in `run.rs` |
| What commands a kind of project needs | `crates/core/src/project.rs` |
| What the page looks like | `ui/styles.css`, `ui/index.html` |
| What the page does | `ui/app.js` and the modules in `ui/parts/` |
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
- **Shell commands are limited, or sandboxed.** Agents may run the commands listed in
  Settings, under Permissions, plus whatever the project's `.claude/settings.json` allows.
  Everything else is refused and shows up in that agent's steps as "Refused or failed".
  With **any command** on, the workers and the reviewer may run anything, inside Claude
  Code's sandbox (macOS's own): they can delete, rename, use git and install packages in
  their copy of the project, but cannot write outside it, read `.env` files or your
  credential folders (`~/.ssh`, `~/.aws` and the like), or reach the network except the
  package registries (PyPI, npm, crates.io). If the sandbox cannot start, the agent does
  not start either. Environment variables are not hidden: keep secrets in files.
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
| A tool line says "Refused or failed" | The command is not on the allow list. The step offers an **Allow** button that adds a rule for commands like it and asks the agent to try again; you can also add it in Settings, under Permissions, or to the project's `.claude/settings.json`. |
| Want to see the raw events without the app | `cargo run -p agent-console-core --example plan -- <folder> "<request>"` |
