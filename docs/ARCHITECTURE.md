# Architecture

## The three parts

```text
+--------------------+   invoke("start_run", ...)   +----------------------+
|  ui/               | ---------------------------> |  src-tauri/          |
|  the page          |                              |  the app shell       |
|  HTML, CSS, JS     | <--------------------------- |  commands + events   |
+--------------------+   "run-event" events         +----------+-----------+
                                                               |
                                                               | plain Rust calls
                                                               v
                                                    +----------------------+
                                                    |  crates/core/        |
                                                    |  the engine          |
                                                    +----------+-----------+
                                                               |
                                                               | starts a process per agent,
                                                               | reads JSON lines from it
                                                               v
                                                         claude  claude  claude
```

- **`ui/`** draws the window's contents. It knows nothing about agents; it shows events.
  `app.js` is the only file the page loads; it imports the modules in `ui/parts/` (one per
  area of the window). Each module imports only from ones listed before it, so the
  dependencies run one way: `tauri`, `dom`, `words`, `markdown`, `state`, `turns`, `cards`,
  `controls`, `project`, `decisions`, `changes`, `history`, `settings`, `prompts`, `app`.
- **`src-tauri/`** is small on purpose. It exposes the commands the page calls, keeps the
  settings files, saves every run (`store.rs`), and forwards every event from the engine to
  the window.
- **`crates/core/`** does the work and has no UI code, so it is tested without a window.

## What happens when you press "Send"

1. `ui/app.js` calls `invoke("start_run", ...)` for the first message of a run, or
   `invoke("send", ...)` after that.
2. The command in `src-tauri/src/lib.rs` checks the action is allowed now
   (`orchestrator::can_message`), starts the work in the background, and returns at once.
3. `Orchestrator::ask` (core) runs the dispatcher through `run_agent`. The dispatcher answers with JSON
   (`Routing` in `config.rs`): answer it itself, or pass it on to brainstorm or to plan.
4. `ClaudeRunner::run_session` starts `claude -p --output-format stream-json ...` in the
   agent's folder (`Run::folder_for`), writes the prompt to its stdin, and reads its stdout
   line by line.
5. Each line is parsed (`claude/protocol.rs`) and turned into an `Event` on the `Run`.
6. The `Run` hands every event to its sink, which emits `run-event` to the window.
7. `handle()` in `ui/app.js` updates the conversation or the matching team card.

Every other button works the same way: a command that checks (`can_approve`,
`can_send_back`, `can_finish_round`...) and then starts one orchestrator method.

## Rounds and worktrees

A `Run` is one working session on one project; it can hold many rounds. A round starts when
you approve a plan (`Orchestrator::build`) and ends with Apply or Discard.

```text
build:   Workspace::create  -> one worktree per worker + one "review" worktree, from HEAD
         workers in parallel, each with its worktree as its folder
         Workspace::commit_worker after each one (and after each chat with a worker)
review:  Workspace::merge_into_review -> on a clash, the resolver (see below)
         checks::run         -> the project's own command, on the merged result
         the reviewer runs in the review worktree; its prompt gets the merged diff and the
         check output; a second review gets only the diff since the first
apply:   Workspace::apply  -> git merge --squash into your project (staged, not committed)
         Workspace::commit -> only when you gave a commit message
         Workspace::remove -> worktrees and branches deleted
```

All git work is in `worktrees.rs`; nothing else runs git. Most tests and the planner-only
example use `Isolation::SharedFolder` instead, where everyone works in the project folder and there is
nothing to merge. `tests/worktree_round.rs` runs a whole round in real worktrees.

## When two workers change the same lines

`merge_into_review` merges one worker branch at a time. If git cannot, it **leaves the merge
in progress**, with the markers in the files, and returns `Merge::Conflicted`. The
orchestrator's `merge_all` then has two ways to go:

- Settings says to resolve: the resolver (`config::resolver`) runs in the review worktree
  with only Read, Edit and Write, and is told which files to mend. Afterwards
  `Workspace::finish_merge` checks every one of those files for leftover markers **before**
  staging anything (`git add` would count a file as resolved, markers and all), then commits
  the merge. The loop tries the whole list again, because git skips branches already in.
- Anything else (it is turned off, the resolver failed, markers are left): `abort_merge`
  puts the review worktree back as it was and the user is told which files clashed. Nothing
  is half-merged, and the project folder was never involved.

The resolver's work is reviewed like any other change, and the user sees every line in the
Changes window before it reaches the project.

## Suggesting commands

`project.rs` reads a project's own files (`Cargo.toml`, `package.json` and its scripts,
`pyproject.toml`, `uv.lock`, `go.mod`, `Makefile`) and returns the allowed-command rules and
check command that kind of project usually needs. It runs nothing and decides nothing: the
Settings window puts the result in the boxes, and the user saves or does not.

## Checks without an agent

If Settings has a check command, `checks.rs` runs it in the review worktree with a timeout,
shortens its output from both ends, and puts it in the reviewer's prompt and in a `checks`
event. The reviewer is told it does not need to run it again, which saves it several turns.

## Saving runs, and picking them up again

Every event is written to `runs/<run id>.jsonl` next to the settings, one JSON object per
line, by the same sink that sends it to the window (`window_sink` in `src-tauri/src/lib.rs`).
The first line says what the run was and how the team was set up for it. `store.rs` reads
these files back for the Runs window, skipping any line that does not parse, so a file cut
short by a crash still opens.

The events say what happened; they are not enough to carry on. For that, `spawn_phase` writes
`Run::snapshot()` to `runs/<run id>.state.json` after every phase: the agent sessions to
resume, the open round, the plan. Opening a saved run (`open_run`) needs three things to go
on with it: that file, the project folder, and, if a round was open, its worktrees
(`Workspace::reopen` checks each one is still a git worktree). Missing any of them, the run
is marked `restored`, every command that would act on it refuses, and the window says which
was missing.

A run that was mid-phase when the app closed comes back as `Stopped`, since whatever was
running died with the app, while its round stays open to apply, discard or review again.

Claude Code finds a session to resume by folder. Worktree paths stay the same for the
whole round, so workers and the reviewer can be resumed until the round closes; then
`Run::close_round` forgets their sessions.

## Talking to agents

Every agent session ends with a session ID, which `run_agent` stores on the `Run` under the
agent's ID. Sending a message (`send_message`, then `Orchestrator::chat`) starts `claude`
again with `--resume=<that session>`, so the agent continues where it left off. While
chatting, agents use the "chat" version of their spec (`config::chat_spec`): same model and
tools, different instructions, and no JSON schema, so they answer in plain text.

Plan after a brainstorm resumes the planner's session too, this time with the plan
schema, so the plan reflects the conversation. The dispatcher and the planner keep their sessions
across rounds; workers and the reviewer only within their round.

## Usage savings

- `claude/command.rs` adds `--disable-slash-commands`, `--strict-mcp-config` and
  `--exclude-dynamic-system-prompt-sections` when the installed `claude` lists them.
  The planner, workers and reviewer have the `Skill` tool, so they do not get
  `--disable-slash-commands`: the planner must see a convention to plan by it. When
  Settings lists plugin folders, those three get `--setting-sources project,local`
  and one `--plugin-dir` per folder instead of the user's installed plugins.
- `claude/command.rs` (`environment`) starts workers, the reviewer and the resolver with
  `CLAUDE_CODE_PROMPT_CACHE_TTL=5m`. Writing to the default one-hour cache costs twice the
  input price, the five-minute one 1.25 times, and these sessions never pause for long.
  The dispatcher and the planner keep the hour: you come back to them after reading.
  Workers also get `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1`: they never commit.

## The sandbox

With "any command" on, workers and the reviewer start with `--settings` from
`claude/sandbox.rs`: Claude Code's sandbox, so every shell command runs under macOS's
Seatbelt. Writes are allowed in the agent's folder, the temp folder and its own git
folder (`.git/worktrees/<name>`, so `git rm` works), and refused in the round's other
folders, the rest of `.git` and everywhere else. Secret files and credential folders
cannot be read (env files are named one by one, so `.env.example` stays writable), and the network reaches only package registries. A command the sandbox
refuses is never retried outside it, and an agent whose `claude` cannot sandbox does not
start. `rm -rf` and its spellings leave the deny list in this mode, since the sandbox
already confines them.
  `claude/locate.rs` reads `claude --help` once, when it finds the program, to know which
  options exist.
- `AgentRunner::parallel_start_gap` makes the orchestrator start parallel agents a few seconds
  apart (`PARALLEL_START_GAP` in `config.rs`), so the later ones can reuse the prompt cache.
  The scripted test runner leaves it at zero.
- One reviewer checks every task at once, and its prompt includes the merged diff
  (`Workspace::review_changes`). A second review resumes its session and only gets the diff
  since the commit it last reviewed.
- Every session has `--max-turns` (from `config.rs`) and `--max-budget-usd` (from Settings).
  When one stops at a limit, `ClaudeRunner` still keeps its session, so a message continues it.
- The `result` event carries the session's token counts (`TokenUsage` in `event.rs`), which
  the page shows per agent and in total. Its cost is only what that call added: a resumed
  session reports everything it has cost so far, so `Run::new_cost` subtracts the total it
  reported last time.

## Settings

`settings.rs` holds what the Settings window changes: model, effort and turn limit for each role,
the spending cap, the allowed shell commands, web search, MCP servers, the number of workers,
whether clashes are resolved, the check command and the notes for every agent. `config.rs` turns them into each agent's `AgentSpec` (for example, an
allowed command `npm test` becomes the permission rule `Bash(npm test)`). The app loads
A project can have a set of its own: `settings::Store` keeps the general set in
`settings.json` (the file older versions wrote) and any project's own in `projects.json`,
keyed by folder. Choosing a project (`use_project`) loads its set. The app loads
them at start-up (`setup` in `src-tauri/src/lib.rs`) into a `SharedSettings`, an
`Arc<RwLock<Settings>>` that every orchestrator holds. The orchestrator reads it each time it
starts an agent, which is why a change applies to the next agent even mid-run. Saving
validates first (model names and allowed commands become command-line arguments, so they are
checked: no leading dash, no commas or brackets in a command, no lone `*`) and writes the file
atomically.

## Events

Events are the only thing that flows from the engine to the page. Each one is a flat
JSON object with a sequence number:

```json
{"seq": 4, "agent": "worker-1", "type": "tool", "name": "Read", "detail": "src/a.py"}
```

The list of event types is the `EventKind` enum in `crates/core/src/event.rs`. The page
shows events strictly in `seq` order. If it notices a gap, or it was just reloaded, it
calls `run_events` to fetch the whole list and shows what it has not shown yet.

## Stopping

Pressing Stop sends a signal that `supervised()` (in `orchestrator.rs`) is waiting on.
The future doing the work is dropped. Dropping it:

- kills each `claude` process (`kill_on_drop`) and everything that process started
  (`ProcessGroupGuard` in `claude/mod.rs`);
- marks each running agent as "stopped" (`StatusGuard` in `runner.rs`).

## Finding `claude`

An app opened from Finder does not get your terminal's PATH. `claude/locate.rs` asks your
login shell for its PATH once, looks for `claude` there and in a few usual places, and
passes that PATH on to `claude` so the tools it runs (`git`, `uv`, `cargo`) are found too.

## Tests

| Where | What it covers |
|---|---|
| `#[cfg(test)]` blocks inside `crates/core/src/*.rs` | Small pieces: config, settings, command line, JSON parsing, PATH handling |
| `crates/core/tests/flow.rs` | The whole flow with scripted agents (`tests/common/scripted.rs`): dispatcher routing, plan, three workers, review, send back, apply, discard, stop, live settings changes |
| `crates/core/tests/worktree_round.rs` | A whole round in real git worktrees, with stand-in agents that write files: the resolver merging a clash, failing to, and being turned off; the check command; apply and commit; a round saved, read back through JSON and then applied, as a restart does |
| `#[cfg(test)]` in `crates/core/src/worktrees.rs` | Every git step on real temporary repositories: commits, clashes left open for the resolver, the per-file diff, apply refusing, committing, cleanup |
| `#[cfg(test)]` in `crates/core/src/checks.rs` | The check command: passing, failing, where it runs, shortening long output |
| `#[cfg(test)]` in `crates/core/src/project.rs` | What each kind of project is offered, and that an unknown one is offered nothing |
| `#[cfg(test)]` in `src-tauri/src/store.rs` | Saved runs: written line by line, read back, a file cut short, the list |
| `crates/core/tests/fake_claude.rs` | `ClaudeRunner` against `tests/fixtures/fake-claude.sh`: real processes, stdin, environment, killing, usage flags, limits |
| `#[cfg(test)]` in `crates/core/src/changes.rs` | Diffs from real temporary git repositories |

Nothing in the tests calls the real `claude`.

## Rust notes for a Python reader

| You will see | What it is | Closest thing in Python |
|---|---|---|
| `struct`, `enum` | Data types. An enum can carry different data per variant. | `@dataclass`, and a tagged union |
| `Option<T>` | "A value or nothing": `Some(x)` or `None`. | `T \| None`, but the compiler makes you handle `None` |
| `Result<T, E>` | "A value or an error": `Ok(x)` or `Err(e)`. | Raising exceptions, made explicit in the return type |
| `?` after an expression | "If this is an error, return it from this function now." | Letting an exception propagate |
| `trait` (`AgentRunner`) | A set of methods a type promises to have. | An abstract base class or a `Protocol` |
| `Arc<T>` | A shared pointer several tasks can hold at once. | Ordinary Python references |
| `Mutex<T>` | Data that must be locked before use, so two tasks never change it at once. | `threading.Lock` around a value |
| `RwLock<T>` | Like `Mutex`, but many readers may hold it at once; a writer waits for them. | A readers-writer lock |
| `..planner(settings)` | "The rest of the fields come from this other value." | `dataclasses.replace(planner, ...)` |
| `async fn`, `.await` | The same idea as in Python. | `async def`, `await` |
| `tokio::select!` | Wait for whichever of several things finishes first. | `asyncio.wait(..., return_when=FIRST_COMPLETED)` |
| `impl Drop` | Code that runs when a value goes away, however that happens. | `try/finally` or a context manager's `__exit__` |
| `#[derive(Serialize)]` | Generates the code to turn the type into JSON. | `dataclasses.asdict` plus `json.dumps` |
| `#[tauri::command]` | Makes a function callable from the page with `invoke`. | A FastAPI route |

The Python version of this app lives in `~/projects/agent-console`. The files map across
almost one to one: `config.py` to `config.rs`, `orchestrator.py` to `orchestrator.rs`,
`runner.py` to `runner.rs` plus `claude/`, `run.py` to `run.rs` and `event.rs`,
`server.py` to `src-tauri/src/lib.rs`.
