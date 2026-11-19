# Build log: `equipment-svc`, made only by Agent Console

**The app:** a service for lending company equipment (laptops, monitors, phones) to employees. Employees
request items, admins approve them, items come back, and overdue loans get flagged.

**Why this app:** it has a real domain with rules, roles, a state machine (request → approve → return)
and a scheduled job. That is enough to need the layered architecture of `nexttech-assistant-svc`.
It runs on SQLite, so no Docker is needed and the sandbox can run it.

**Project:** `~/equipment-svc`. It starts as an empty repo with one README commit, `aa0cf40 Start`.

## Setup (once)

1. Rebuild Agent Console from `main` so the cost fix is in: `cargo tauri dev`.
2. Open `~/equipment-svc` and use these Settings for the project:
   - **Preset:** Balanced (Opus planner, Haiku workers with Sonnet for hard tasks, Sonnet reviewer), 3 workers
   - **Any command:** on, so agents can run `uv`, the tests and the app inside the sandbox
   - **Plugins:** `dev-conventions`. Its `layered-service` skill, the linter hook and the `.env` guard are part of the story.
   - **Check command:** leave it empty for step 1. The plan card offers `uv run pytest` once the project has one.
3. After each step, run the report and paste its output under that step:

```bash
python3 scripts/run_report.py ~/equipment-svc
```

The report covers the whole run, so for each step copy only its own section. The script can't read time,
so write the start and end time yourself.

## The architecture we ask for

The same layering as `nexttech-assistant-svc`:

```
api/routes → services → repositories → SQLite (SQLAlchemy async)
                ↘ models (Pydantic DTOs)   schemas (SQLAlchemy entities)   mappers (from_x_to_y)
exceptions: one AppError carrying an AppErrorCode (code, HTTP status, message)
```

---

## Step 1: Foundation (Plan)

**Shows:** the planner setting up a project from nothing, and writing the rules every later step follows.

```text
Start a new Python backend, managed with uv (Python 3.13), called equipment-svc: a service for lending company equipment to employees. This step only sets up the foundation; the features come later.

Use a layered architecture, with the package in src/equipment_svc/:
- api/: app.py (creates the FastAPI app), lifespan.py (builds long-lived objects once and keeps them on app.state), dependencies.py (get_x(request) functions, services built per request), exception_handlers.py, routes/<resource>_routes.py
- services/: XService classes holding the business rules and transactions
- repositories/: one XRepository per table, only database access
- schemas/: SQLAlchemy 2 entities (async, SQLite through aiosqlite), all re-exported from schemas/__init__.py for Alembic
- models/: Pydantic DTOs and enums, one file per business unit. Enum values are UPPER.
- mappers/: from_x_to_y() functions between entities and DTOs
- exceptions/: one AppError exception carrying an AppErrorCode enum, where each member holds a code, an HTTP status and a message. One handler turns it into {"code", "message"} JSON.
- configuration/: settings with pydantic-settings (database URL, JWT secret, loan limits), read from environment variables, with a .env.example

Routes never touch the database, and repositories never raise HTTP errors.

For now add: the database session, Alembic set up with an empty first migration, a GET /health route returning {"status": "OK"}, and pytest with a fixture that gives each test a fresh temporary database and an httpx AsyncClient. Add a Makefile with sync, run, test, lint (ruff) and migrate.

Write these layering rules into a short CLAUDE.md so later changes follow them, and a README that says how to install, run and test it.
```

| | |
|---|---|
| Started / finished | 10:57 / 11:02 (5 min) |
| What I did by hand (Allow, chats, send back) | Sent back twice; applied with "needs changes" (41 files committed) |
| Notes | See "What went wrong" below |

**Result: Plan, 1 task, $0.32.** Checks passed 3/3 (`uv run pytest -q`, found by the app). Reviews: needs changes ×3. Tool calls 56, refused 0.

| Agent | Model | Turns | Cost |
|---|---|---|---|
| planner | Opus 5.5 (low effort: "your request names the files") | 5 | $0.199 |
| reviewer | Sonnet 5.5 | 9 | $0.095 |
| worker-1 | Haiku 5.5 | 55 | $0.029 |

Tokens: 56 input, 32,696 output, 938,147 cache read, 100,377 cache write.

**What went wrong, and what changed in Agent Console because of it**

1. **`.env.example` could not be written.** The sandbox denied `Read(**/.env.*)` to keep secrets out.
   In Claude Code a Read deny also blocks Write and Edit, and an allow rule can't undo it, so there
   was no Allow button. `.env.*` also matched the placeholder file. The worker correctly refused to
   work around it and wrote the file's contents into its report.
   → **Fixed in Agent Console:** the env files that really hold secrets are now denied by name
   (`.env`, `.env.local`, `.env.production`…), so `.env.example` can be written.
2. **Two send-backs were wasted.** A send-back can't fix a permission block: each one cost a worker turn and a
   new review for "nothing changed". Lesson: when the review says "permission block, not a code
   problem", don't send it back. Fix the permission, or apply and add the file by hand.
3. **The server the worker started.** It ran `make run` to try `/health` (200 `{"status":"OK"}`). The
   sandbox refused `pkill`, because it can't list processes, so the worker reported "please kill PID
   75695". When the round was applied, Agent Console's cleanup stopped it. Nothing was left on port 8000.

What worked: low-effort planning cut the planner's cost; the app found `uv run pytest -q` on its own;
the worker checked lint, tests, migrations and a real `curl` before reporting.

`.env.example` is still missing from the project. Step 1b adds it.

---

## Step 1b: Add the missing `.env.example` (Quick)

Run it after restarting Agent Console with the sandbox fix.

```text
Add .env.example listing every setting in configuration/ with safe local values: DATABASE_URL=sqlite+aiosqlite:///./equipment.db, JWT_SECRET=change-me-local-development-only, JWT_ALGORITHM=HS256, JWT_EXPIRE_MINUTES=60, MAX_ACTIVE_LOANS_PER_EMPLOYEE=3, MAX_LOAN_DAYS=14, each with a one-line comment.
```

<!-- report section for step 1b -->

---

## Step 2: Users and login (Plan)

**Shows:** a feature that cuts through every layer, and security rules (hashing, roles).

```text
Add users and login.

A user has an email (unique), a full name, a hashed password (argon2 through pwdlib) and a role: EMPLOYEE or ADMIN.

- POST /auth/login takes email and password and returns a JWT access token (PyJWT, expires after the number of minutes in settings). A wrong email and a wrong password give the same INVALID_CREDENTIALS error.
- GET /users/me returns the current user.
- POST /users (ADMIN only) creates a user. A duplicate email gives EMAIL_TAKEN.
- A get_current_user dependency reads the bearer token, and a require_admin dependency gives FORBIDDEN to anyone else.
- A CLI command `uv run equipment-svc create-admin <email> <name>` that asks for the password, so the first admin can be made.

Follow CLAUDE.md for the layers. Add an Alembic migration. Tests: login works, wrong password, unknown email, expired token, missing token, an employee creating a user is refused, duplicate email.
```

| | |
|---|---|
| Started / finished | |
| What I did by hand | |
| Notes | |

<!-- report section for step 2 -->

---

## Step 3: Equipment catalogue (Plan)

**Shows:** a step a good plan splits into parallel tasks.

```text
Add the equipment catalogue.

An item has a name, a category (LAPTOP, MONITOR, PHONE, ACCESSORY), a serial number (unique), notes, and a status: AVAILABLE, ON_LOAN or RETIRED. New items are AVAILABLE.

- ADMIN only: create an item, edit its name, category and notes, and retire it. A retired item cannot come back, and an item ON_LOAN cannot be retired (ITEM_ON_LOAN).
- Any logged-in user: list items with optional filters for category and status, paginated (limit and offset, with the total count), and get one item. A missing item gives ITEM_NOT_FOUND.
- A duplicate serial number gives SERIAL_TAKEN.

Follow CLAUDE.md. Add a migration. Tests for every rule above, and that an employee cannot create, edit or retire.
```

| | |
|---|---|
| Started / finished | |
| What I did by hand | |
| Notes | |

<!-- report section for step 3 -->

---

## Step 4: Loans, the core rules (Plan)

**Shows:** the hardest step. Expect a task marked hard (so it runs on Sonnet), maybe a merge conflict for the resolver, and the reviewer earning its cost.

```text
Add loans: an employee borrows an item and gives it back.

A loan has the item, the borrower, a requested due date, and a status: REQUESTED, APPROVED, REJECTED, RETURNED. It records when it was requested, decided and returned, and who decided.

- POST /loans: an employee asks for an AVAILABLE item until a due date, at most MAX_LOAN_DAYS (the existing setting) days ahead and not in the past (INVALID_DUE_DATE). An item that is not AVAILABLE gives ITEM_NOT_AVAILABLE. An employee who already has MAX_ACTIVE_LOANS_PER_EMPLOYEE (the existing setting) requested or approved loans gives LOAN_LIMIT_REACHED.
- ADMIN: approve (the item becomes ON_LOAN; if two requests wait for the same item, approving one rejects the others), reject with a reason, and mark returned (the item becomes AVAILABLE again).
- Only these moves are allowed: REQUESTED → APPROVED or REJECTED, APPROVED → RETURNED. Anything else gives INVALID_LOAN_STATE.
- GET /loans/me lists my loans. GET /loans (ADMIN) lists all of them, filtered by status. GET /items/{id}/history lists one item's loans, newest first.
- Each change of a loan's state and its item's status happens in one transaction.

Follow CLAUDE.md, add a migration, and test every rule and every forbidden move.
```

| | |
|---|---|
| Started / finished | |
| What I did by hand | |
| Notes | |

<!-- report section for step 4 -->

---

## Step 5: Overdue loans and a scheduled job (Plan, with running the app)

**Shows:** a background job, and the sandbox. The worker starts the real app and calls it.

```text
Flag overdue loans.

- A loan is overdue when it is APPROVED and its due date has passed.
- GET /loans/overdue (ADMIN) lists overdue loans with the borrower and how many days late, most late first.
- A daily job (APScheduler, started and stopped in lifespan.py, time from settings) writes a notification for each borrower with an overdue loan, at most one per loan per day. GET /notifications/me lists mine, newest first, and POST /notifications/{id}/read marks one read.
- Put the overdue rule in one service method, used by both the endpoint and the job, so the job can be tested by calling it directly with a fixed date.

Follow CLAUDE.md, add a migration and tests (not overdue on the due date itself, overdue the day after, a returned loan is never overdue, one notification per loan per day).

Then check it for real: start the app on port 8765 with a temporary database, create an admin, an employee and an item, make an overdue loan by setting its due date in the database, call GET /loans/overdue with curl, and put the response in your report. Stop the server when you are done.
```

| | |
|---|---|
| Started / finished | |
| What I did by hand | |
| Notes | |

<!-- report section for step 5 -->

---

## Step 6: A question (Ask)

**Shows:** the dispatcher answering by itself, with no planner and no workers.

```text
Where is the rule that stops two approved loans on one item, and which error does the user get?
```

<!-- report section for step 6 -->

## Step 7: A small change (Quick)

**Shows:** Ask sending a small, clear change straight to one worker.

```text
Change the message of LOAN_LIMIT_REACHED to "You already have the maximum number of items on loan".
```

<!-- report section for step 7 -->

---

## Summary (fill in at the end)

| Step | Route | Tasks | Reviews | Refused | Cost | Time |
|---|---|---|---|---|---|---|
| 1 Foundation | | | | | | |
| 2 Users and login | | | | | | |
| 3 Catalogue | | | | | | |
| 4 Loans | | | | | | |
| 5 Overdue + job | | | | | | |
| 6 Question | | | | | | |
| 7 Small change | | | | | | |
| **Total** | | | | | | |

What to look at once it's done:
- Lines of code and tests: `cd ~/equipment-svc && git ls-files '*.py' | xargs wc -l | tail -1`
- Does it follow the layers? Ask in Agent Console: "Does any route touch the database, or any repository raise an HTTP error?"
- Which step cost the most, and was it the planner again?
