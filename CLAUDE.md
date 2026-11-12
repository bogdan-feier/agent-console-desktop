# Project guide for Claude

Agent Console: a macOS desktop app (Tauri 2, Rust) that runs a planner (Opus), two
workers (Sonnet) and two reviewers (Sonnet) on a project by driving the `claude` CLI.

## Commands
- Run from source: `make dev` (demo mode: `make demo`)
- Tests: `make test`
- Format and lint: `make fmt`, `make lint` (clippy warnings are errors)
- Build the .app: `make build`

## Layout
- `crates/core/` is the engine. No Tauri, no UI. Everything here is unit-testable.
  - `config.rs`: models, permissions, role instructions. Tunables go here as named constants.
  - `orchestrator.rs`: the plan, build, review flow.
  - `claude/`: the only code that starts or parses the `claude` program.
- `src-tauri/` is the app shell: commands the page calls, and event forwarding. Keep it thin.
- `ui/` is the page: plain HTML, CSS and JavaScript. No framework, no build step, no CDN.
- `docs/ARCHITECTURE.md` explains how the pieces talk to each other.

## Rules
- New behaviour goes in `crates/core` with a test. `src-tauri` only wires it to the page.
- Do not call `claude` from anywhere except `crates/core/src/claude/`.
- An event the page receives is a variant of `EventKind` in `event.rs`; if you add one,
  handle it in `handle()` in `ui/app.js`.
- No `unwrap()` on anything that can fail at runtime in non-test code; return an error the
  user can read. `expect()` is fine for things that are true by construction.
- No `unsafe`.
- Prefer simple, readable code over clever code. Comments explain why, not what.
- The owner is learning Rust: when you use a Rust feature for the first time in a file
  (traits, lifetimes, `select!`, `Drop`), add one or two lines saying what it does.

## Git
- Conventional commit prefixes: `feat`, `fix`, `refactor`, `chore`, `docs`, `test`.
- One logical change per commit. Never force-push; ask before any `git push`.
