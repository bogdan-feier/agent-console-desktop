# Project guide for Claude

Agent Console: a macOS desktop app (Tauri 2, Rust) that runs a team on one project by
driving the `claude` CLI: a dispatcher (Haiku) answers or routes each message, a planner (Opus)
brainstorms and plans, up to 3 workers (Sonnet) build in their own git worktrees, and one
reviewer checks all their work merged together. When two workers change the same lines, a
resolver merges them. The user applies or discards each round.

## Commands
- Run from source: `cargo tauri dev`
- Tests: `cargo test --workspace`
- Format: `cargo fmt --all`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings` (warnings are errors)
- Build the .app: `cargo tauri build`

## Layout
- `crates/core/` is the engine. No Tauri, no UI. Everything here is unit-testable.
  - `config.rs`: default models, permissions, role instructions. Tunables go here as named constants.
  - `settings.rs`: what the Settings window changes (model, effort and turn limit per role, budget, allowed commands, web search, MCP, number of workers, notes for every agent), presets, and the checks on each value.
  - `changes.rs`: a git diff for the reviewer's prompt (shared-folder mode).
  - `checks.rs`: runs the project's own check command on the merged changes. No agent involved.
  - `project.rs`: reads a project's own files to suggest the commands its agents will need.
  - `worktrees.rs`: every git operation for a round: create, commit, merge (with conflicts
    left in place for the resolver), the per-file diff, apply, commit, remove. Nothing else
    runs git.
  - `orchestrator.rs`: the plan, build, review flow.
  - `claude/`: the only code that starts or parses the `claude` program.
- `src-tauri/` is the app shell: commands the page calls, and event forwarding. Keep it thin.
  - `store.rs`: saves every run to `runs/<id>.jsonl`, and where it got to in
    `runs/<id>.state.json`, so a run can be read, and usually continued, after a restart.
- `ui/` is the page: plain HTML, CSS and JavaScript. No framework, no build step, no CDN.
  - `app.js` wires it together; `ui/parts/*.js` are ES modules it imports, one per area of
    the window (`turns.js`, `cards.js`, `controls.js`, `decisions.js`, `changes.js`,
    `history.js`, `settings.js`, `project.js`, `prompts.js`, plus `dom.js`, `markdown.js`,
    `state.js`, `words.js`, `tauri.js`). Imports only ever point one way, from the later
    files to the earlier ones, so there are no cycles.
- `docs/ARCHITECTURE.md` explains how the pieces talk to each other.

## Rules
- Keep agent sessions lean: anything added to every prompt costs usage on every turn of
  every agent. Prefer putting information in the one prompt that needs it.
- New behaviour goes in `crates/core` with a test. `src-tauri` only wires it to the page.
- Do not call `claude` from anywhere except `crates/core/src/claude/`.
- An event the page receives is a variant of `EventKind` in `event.rs`; if you add one,
  handle it in `handle()` in `ui/app.js`. Events are also written to disk, so every variant
  must deserialise as well as serialise.
- No `unwrap()` on anything that can fail at runtime in non-test code; return an error the
  user can read. `expect()` is fine for things that are true by construction.
- No `unsafe`.
- Prefer simple, readable code over clever code. Comments explain why, not what.
- The owner is learning Rust: when you use a Rust feature for the first time in a file
  (traits, lifetimes, `select!`, `Drop`), add one or two lines saying what it does.

## Checks
GitHub Actions runs `cargo fmt --check`, clippy and the tests on every push
(`.github/workflows/ci.yml`), plus `node --check` on each UI module.

## Git
- Conventional commit prefixes: `feat`, `fix`, `refactor`, `chore`, `docs`, `test`.
- One logical change per commit. Never force-push; ask before any `git push`.
