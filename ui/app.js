// The page. It talks to the Rust side in two ways:
//   invoke("command_name", {args})  calls a #[tauri::command] function and awaits its result
//   listen("run-event", handler)    receives events the Rust side emits while agents work
// Both come from window.__TAURI__, which Tauri injects (see "withGlobalTauri" in tauri.conf.json).
//
// The window has two halves:
//   the conversation (left): you, the dispatcher and the planner, plus the cards where
//     you decide things (approve a plan, act on a review);
//   the team (right): one card per worker and one for the reviewer, showing their work.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STATUS_WORDS = { idle: "Waiting", running: "Working", done: "Finished", error: "Stopped early", stopped: "Stopped" };
const STATE_WORDS = {
  ready: "Ready for your next message",
  chatting: "An agent is replying",
  planning: "The planner is writing a plan",
  awaiting_approval: "Waiting for you to approve the plan",
  building: "Workers are building",
  reviewing: "The reviewer is checking",
  done: "Reviewed: apply, discard, or send work back",
  failed: "Stopped on an error",
  stopped: "Stopped",
};
const CARD_EMPTY = {
  worker: "Gets a task when you approve a plan, and works in its own copy of the project.",
  reviewer: "Checks all the workers' changes together when they finish. Then you decide what happens to them.",
};
const PLACEHOLDER = {
  dispatcher: "Ask a question or describe a change",
  brainstorm: "Think an idea through with the planner",
  plan: "Anything the plan should take into account (optional)",
  planFirst: "Describe the change you want planned",
};
const HUE = { dispatcher: "plan-hue", planner: "plan-hue", worker: "build-hue", reviewer: "review-hue" };
const BUSY_STATES = ["chatting", "planning", "building", "reviewing"];
const REST_STATES = ["ready", "awaiting_approval", "done", "stopped", "failed"];
const STAGES = ["ask", "plan", "approve", "build", "review", "apply"];
const AUTH_WORDS = { claude_login: "Signed in with Claude Code", api_key: "Billing: API key" };
const MODEL_CHOICES = [["opus", "Opus"], ["sonnet", "Sonnet"], ["haiku", "Haiku"]];
const PRESET_WORDS = {
  lean: ["Lean", "Least usage: Sonnet plans, low effort for workers and reviewer"],
  balanced: ["Balanced", "The default: Opus plans, Sonnet builds and reviews, medium effort"],
  thorough: ["Thorough", "Most careful and most usage: high effort, Opus also reviews"],
};
const EFFORT_CHOICES = [["", "Default"], ["low", "Low"], ["medium", "Medium"], ["high", "High"], ["xhigh", "Extra high"], ["max", "Max"]];
const EXAMPLES = [
  ["dispatcher", "How is this project organised?"],
  ["dispatcher", "Add input validation to the signup form, with tests"],
  ["brainstorm", "What would make the test suite faster?"],
];

const $ = (id) => document.getElementById(id);
const agents = {};     // agent id -> { id, role, label, number, card?, turn?, hasWorked }
let settings = null;   // the current settings, as the Rust side last confirmed them
let defaults = null;   // the built-in defaults, for "Reset to defaults"
let folder = "";       // the project folder chosen in the top bar
let runId = null;      // the run this window is showing
let runFolder = null;  // the folder that run was started with
let runState = null;
let roundOpen = false; // true while a round's changes wait to be applied or discarded
let lastStage = 0;     // where the round got to, kept when it is stopped
let verdict = null;    // the reviewer's latest verdict in the open round
let nextSeq = 0;       // sequence number of the next event we expect for this run
let runCost = 0;       // API-equivalent dollars the agents of this run have used
let runTokens = 0;     // tokens the agents of this run have used, all kinds together

// --- Small helpers -----------------------------------------------------------

function remember(key, value) { try { localStorage.setItem(key, value); } catch (e) {} }
function recall(key) { try { return localStorage.getItem(key); } catch (e) { return null; } }
function forget(key) { try { localStorage.removeItem(key); } catch (e) {} }

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(text, onClick, className) {
  const node = el("button", className || "", text);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
}

// Escape first, then allow `code` and **bold**, so agent text can never inject HTML.
function richText(text) {
  const escaped = text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  return escaped.replace(/`([^`\n]+)`/g, "<code>$1</code>").replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
}

function rich(tag, text, className) {
  const node = el(tag, className);
  node.innerHTML = richText(text);
  return node;
}

// Commands fail with a plain string (Rust's Err(String)).
function problemText(error) { return typeof error === "string" ? error : String(error.message || error); }
function showProblem(error) { $("notice").textContent = problemText(error); }
function clearProblem() { $("notice").textContent = ""; }

const isBusy = () => BUSY_STATES.includes(runState);
const isAtRest = () => runId !== null && REST_STATES.includes(runState);
const mode = () => document.querySelector("input[name=mode]:checked").value;
const isChat = (agent) => agent.role === "dispatcher" || agent.role === "planner";

// "opus" plus "high" -> "opus, high effort"
function modelLabel(role) {
  const chosen = settings[role];
  return chosen.effort ? chosen.model + ", " + chosen.effort + " effort" : chosen.model;
}

// Append to a scrolling box, following along only if the reader was already at the bottom.
function append(box, node) {
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 80;
  box.append(node);
  if (nearBottom) box.scrollTop = box.scrollHeight;
}

// --- The project ------------------------------------------------------------------

function setFolder(path) {
  folder = path || "";
  remember("agent-console-folder", folder);
  const name = folder.replace(/\/+$/, "").split("/").pop();
  $("project-name").textContent = name || "Choose a project";
  $("project-path").textContent = folder.replace(/^\/Users\/[^/]+/, "~");
  $("project").title = folder ? folder + "\nClick to choose another project" : "Choose the project the team works on";
  refreshWelcome();
}

// --- The conversation ----------------------------------------------------------------

const thread = () => $("thread");

function addToThread(node) {
  const welcome = thread().querySelector(".welcome");
  if (welcome) welcome.remove();
  append(thread(), node);
}

// What an empty conversation shows: where to start, with a few examples to try.
function refreshWelcome() {
  if (runId !== null) return;
  const box = el("div", "welcome");
  box.append(el("h2", "", folder ? "What should the team work on?" : "Start by choosing a project"));
  box.append(el("p", "", folder
    ? "Ask a question and the dispatcher answers it. Ask for a change and the planner splits it into tasks. Once you approve them, the workers build in separate copies of the project and the reviewer checks their work. Nothing reaches your project until you apply it."
    : "Pick the folder of a git project in the top left. The team works on that one project."));
  if (folder) {
    const examples = el("div", "examples");
    for (const [exampleMode, text] of EXAMPLES) {
      examples.append(button(text, () => {
        document.querySelector("input[name=mode][value=" + exampleMode + "]").checked = true;
        $("message").value = text;
        refreshPlaceholder();
        $("message").focus();
      }));
    }
    box.append(examples);
  } else {
    box.append(button("Choose a project", chooseFolder, "primary"));
  }
  thread().replaceChildren(box);
}

function youSaid(agent, text) {
  if (agent.card) return addToCard(agent, el("div", "you", text));
  // When the dispatcher hands a message on, the planner receives the same words:
  // show them once. (Only agent turns may stand between the two.)
  const nodes = [...thread().children];
  const last = nodes.filter((node) => node.matches(".you")).pop();
  if (last && last.textContent === text && nodes.slice(nodes.indexOf(last) + 1).every((node) => node.matches(".turn"))) return;
  addToThread(el("div", "you", text));
}

// One agent turn in the conversation or on a card: a heading, the steps it took
// (folded away), and what it said.
function startTurn(agent, running) {
  const turn = el("div", "turn " + HUE[agent.role]);
  const head = el("div", "turn-head");
  const state = running ? el("span", "working", "working") : el("span");
  head.append(el("span", "who", agent.label), el("span", "model", modelLabel(agent.role)), state);
  const steps = el("details", "steps");
  steps.hidden = true;
  const summary = el("summary");
  const list = el("ol");
  steps.append(summary, list);
  turn.append(head, steps);
  const made = { root: turn, state, steps, summary, list, count: 0, last: "", running };
  if (isChat(agent)) addToThread(turn); else addToCard(agent, turn);
  if (running) agent.turn = made; else agent.lastTurn = made;
  return made;
}

// Where something an agent says goes. Some of it arrives just after its session ended
// (the dispatcher's reply, a review): that belongs to the turn that just finished,
// as long as nothing else has been shown since.
function turnOf(agent) {
  if (agent.turn) return agent.turn;
  const last = agent.lastTurn;
  if (last && last.root.parentElement && !last.root.nextElementSibling) return last;
  return startTurn(agent, false);
}

function addToTurn(agent, node) {
  const turn = turnOf(agent);
  turn.root.append(node);
  const box = isChat(agent) ? thread() : agent.card.log;
  if (box.scrollHeight - box.scrollTop - box.clientHeight < 160) box.scrollTop = box.scrollHeight;
}

function addStep(agent, node, text) {
  const turn = turnOf(agent);
  turn.count += 1;
  turn.last = text;
  turn.list.append(node);
  turn.steps.hidden = false;
  refreshSteps(turn, turn.running);
}

function refreshSteps(turn, running) {
  const word = turn.count === 1 ? " step" : " steps";
  turn.summary.textContent = running ? turn.last : turn.count + word;
  turn.summary.title = running ? turn.count + word + " so far" : "";
}

function endTurn(agent, status) {
  const turn = agent.turn;
  if (!turn) return;
  turn.state.className = "";
  turn.state.textContent = status === "done" ? "" : STATUS_WORDS[status] || status;
  turn.running = false;
  refreshSteps(turn, false);
  agent.turn = null;
  agent.lastTurn = turn;
}

// --- The team cards ------------------------------------------------------------------

function buildCards(list) {
  for (const info of list) {
    const agent = { ...info, number: Number(info.id.split("-")[1]) || 0, turn: null, hasWorked: false };
    agents[info.id] = agent;
    if (isChat(agent)) continue;

    const card = el("section", "card " + HUE[agent.role]);
    const header = el("header");
    const titleRow = el("div", "title-row");
    const status = el("span", "status");
    status.append(el("span", "dot"), el("span", "status-word"));
    titleRow.append(el("h2", "", agent.label), status);
    const task = el("div", "task-title");
    header.append(titleRow, el("div", "model"), task);
    const log = el("div", "log");

    const footer = el("footer");
    const form = el("form", "message-form");
    const input = el("textarea");
    input.rows = 1;
    input.placeholder = agent.role === "reviewer" ? "Message the reviewer" : "Message " + agent.label;
    input.setAttribute("aria-label", "Message " + agent.label);
    const send = el("button", "", "Send");
    send.type = "submit";
    form.append(input, send);
    form.addEventListener("submit", (submitted) => { submitted.preventDefault(); sendToAgent(agent, input); });
    input.addEventListener("keydown", (key) => {
      if (key.key === "Enter" && (key.metaKey || key.ctrlKey)) { key.preventDefault(); form.requestSubmit(); }
    });
    footer.append(form);

    card.append(header, log, footer);
    agent.card = { root: card, log, task, model: header.querySelector(".model"), word: status.querySelector(".status-word"), form, input };
    (agent.role === "reviewer" ? $("reviewer-slot") : $("workers")).append(card);
  }
}

function addToCard(agent, node) {
  const empty = agent.card.log.querySelector(".empty");
  if (empty) empty.remove();
  append(agent.card.log, node);
}

function clearCards() {
  for (const agent of Object.values(agents)) {
    agent.turn = null;
    agent.lastTurn = null;
    if (!agent.card) continue;
    agent.card.log.replaceChildren(el("p", "empty", CARD_EMPTY[agent.role]));
    agent.card.task.textContent = "";
    agent.hasWorked = false;
    setStatus(agent, "idle");
  }
  refreshTeam();
}

function setStatus(agent, status) {
  // "error" can mean it stopped at a turn limit or the cap; its session is kept, so it
  // can be told to continue. (If it never started, the Rust side says so.)
  if (status === "done" || status === "error") agent.hasWorked = true;
  if (!agent.card) return;
  agent.card.root.dataset.status = status;
  agent.card.word.textContent = STATUS_WORDS[status] || status;
  if (status === "idle") agent.card.model.textContent = modelLabel(agent.role);
}

// Workers take their task from the plan that was approved.
function showTaskTitles() {
  const plan = [...thread().querySelectorAll(".decision.plan:not(.superseded)")].pop();
  const titles = plan ? [...plan.querySelectorAll("[data-field=title]")].map((field) => field.value) : [];
  for (const agent of Object.values(agents)) {
    if (agent.role !== "worker") continue;
    const title = titles[agent.number - 1];
    agent.card.task.textContent = title ? "Task " + agent.number + ": " + title : "No task this round";
    agent.card.task.title = title || "";
  }
  refreshTeam();
}

// Workers above the "most workers per plan" setting sit out; dim their cards while idle.
function refreshTeam() {
  for (const agent of Object.values(agents)) {
    if (agent.role !== "worker") continue;
    const off = agent.number > settings.worker_count && agent.card.root.dataset.status === "idle";
    agent.card.root.classList.toggle("off", off);
    const empty = agent.card.log.querySelector(".empty");
    if (empty) {
      empty.textContent = off
        ? "Not used: Settings allows " + settings.worker_count + (settings.worker_count === 1 ? " worker" : " workers") + " per plan."
        : agent.card.task.textContent === "No task this round"
          ? "The plan has fewer tasks than there are workers, so this one sits out."
          : CARD_EMPTY.worker;
    }
  }
}

function refreshModelLabels() {
  for (const agent of Object.values(agents)) {
    if (agent.card && agent.card.root.dataset.status === "idle") agent.card.model.textContent = modelLabel(agent.role);
  }
}

// --- State of the run ------------------------------------------------------------------

function setRunState(state) {
  runState = state;
  $("run-state").textContent = state ? STATE_WORDS[state] : "No run yet";
  if (state === "building") { lockPlans(); showTaskTitles(); }
  refreshStages();
  refreshControls();
}

function setRound(open) {
  roundOpen = open;
  if (open) {
    clearCards();
    verdict = null;
  } else {
    // The workers' and reviewer's sessions ended with the round.
    for (const agent of Object.values(agents)) if (agent.card) agent.hasWorked = false;
    for (const old of thread().querySelectorAll(".decision.review button")) old.remove();
  }
  refreshStages();
  refreshControls();
}

// Light up the step of the round we are at.
function refreshStages() {
  let stage = 0;
  if (runState === "planning") stage = 1;
  else if (runState === "awaiting_approval") stage = 2;
  else if (runState === "building") stage = 3;
  else if (runState === "reviewing") stage = 4;
  else if (roundOpen) stage = runState === "chatting" ? lastStage : 5;
  const halted = runState === "stopped" || runState === "failed";
  if (halted) stage = lastStage; else lastStage = stage;
  $("stages").querySelectorAll("li").forEach((item, index) => {
    item.classList.toggle("passed", runId !== null && index < stage);
    item.classList.toggle("current", runId !== null && index === stage);
    item.classList.toggle("halted", halted && index === stage);
    if (index === stage) item.setAttribute("aria-current", "step"); else item.removeAttribute("aria-current");
  });
}

// Enable exactly what can be done right now.
function refreshControls() {
  const busy = isBusy();
  const rest = isAtRest();
  $("send").hidden = busy;
  $("stop").hidden = !busy;
  $("new-run").disabled = busy || runId === null;
  $("project").disabled = busy;
  for (const agent of Object.values(agents)) {
    if (!agent.card) continue;
    // Workers and the reviewer can be messaged during the round they worked in.
    const usable = rest && agent.hasWorked && roundOpen;
    for (const control of agent.card.form.querySelectorAll("textarea, button")) control.disabled = !usable;
  }
  // Showing the round bar makes the conversation shorter; keep its end in view.
  const box = thread();
  const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  $("round-bar").hidden = !(roundOpen && rest);
  if (atEnd) box.scrollTop = box.scrollHeight;
  $("round-text").textContent = verdict === "approve"
    ? "The reviewer approved these changes."
    : verdict === "needs_changes" ? "The reviewer asked for changes." : "The workers' changes are ready.";
  for (const node of thread().querySelectorAll("[data-needs]")) {
    node.disabled = node.dataset.needs === "approval" ? runState !== "awaiting_approval" || roundOpen : !(rest && roundOpen);
  }
  refreshPlaceholder();
}

function refreshPlaceholder() {
  const chosen = mode();
  $("message").placeholder = chosen === "plan" && runId === null ? PLACEHOLDER.planFirst : PLACEHOLDER[chosen];
}

// --- Sending ---------------------------------------------------------------------------

// The main message box. A message continues the current run if it is for the same
// folder; otherwise it starts a new run.
async function sendMain() {
  const message = $("message").value.trim();
  const chosen = mode();
  clearProblem();
  if (!folder) return showProblem("Choose a project first.");
  try {
    if (runId !== null && folder === runFolder) {
      if (chosen === "plan") await invoke("make_plan", { runId, guidance: message || null });
      else if (!message) return;
      else await invoke("send", { runId, message, mode: chosen });
    } else {
      if (!message) return;
      const id = await invoke("start_run", { projectDir: folder, message, mode: chosen });
      runFolder = folder;
      remember("agent-console-run-folder", folder);
      await show(id);
    }
    $("message").value = "";
  } catch (error) { showProblem(error); }
}

async function sendToAgent(agent, input) {
  const message = input.value.trim();
  if (!message || !runId) return;
  clearProblem();
  try {
    await invoke("send_message", { runId, agent: agent.id, message });
    input.value = "";
  } catch (error) { showProblem(error); }
}

function roundAction(command) {
  clearProblem();
  invoke(command, { runId }).catch(showProblem);
}

async function chooseFolder() {
  const picked = await invoke("pick_folder").catch(() => null);
  if (picked) setFolder(picked);
}

// --- The plan and the review ------------------------------------------------------------

function lockPlans() {
  for (const plan of thread().querySelectorAll(".decision.plan")) {
    for (const node of plan.querySelectorAll("button, .hint")) node.remove();
    for (const field of plan.querySelectorAll("input, textarea")) field.readOnly = true;
  }
}

function showPlan(event) {
  // A newer plan replaces any earlier one that was never approved.
  for (const older of thread().querySelectorAll(".decision.plan")) older.classList.add("superseded");
  lockPlans();

  const box = el("div", "decision plan plan-hue");
  box.append(el("h3", "", "Plan: " + event.tasks.length + (event.tasks.length === 1 ? " task" : " tasks")));
  box.append(rich("p", event.summary));
  event.tasks.forEach((task, index) => {
    const number = index + 1;
    const card = el("div", "task");
    card.append(el("div", "goes-to", "Task " + number + ", for Worker " + number));
    const title = el("input"); title.type = "text"; title.value = task.title; title.dataset.field = "title";
    title.setAttribute("aria-label", "Task " + number + " title");
    const instructions = el("textarea"); instructions.value = task.instructions; instructions.dataset.field = "instructions";
    instructions.setAttribute("aria-label", "Task " + number + " instructions");
    const files = el("input", "files"); files.type = "text"; files.value = task.files.join(", "); files.dataset.field = "files";
    files.setAttribute("aria-label", "Files for task " + number + ", separated by commas");
    card.append(title, instructions, files);
    box.append(card);
  });
  const approve = button("Approve and start the workers", async () => {
    const tasks = [...box.querySelectorAll(".task")].map((card) => ({
      title: card.querySelector("[data-field=title]").value,
      instructions: card.querySelector("[data-field=instructions]").value,
      files: card.querySelector("[data-field=files]").value.split(",").map((f) => f.trim()).filter(Boolean),
    }));
    clearProblem();
    try { await invoke("approve_plan", { runId, tasks }); } catch (error) { showProblem(error); }
  }, "primary");
  approve.dataset.needs = "approval";
  approve.style.justifySelf = "start";
  box.append(approve);
  box.append(el("p", "hint", "You can edit the tasks above before approving. For a different plan, choose Plan below, say what to change, and send."));
  addToThread(box);
  refreshControls();
  reveal(box);
}

function showReview(agent, event) {
  for (const old of thread().querySelectorAll(".decision.review button")) old.remove();
  verdict = event.verdict;

  const box = el("div", "decision review review-hue verdict-" + event.verdict);
  box.append(el("h3", "", event.verdict === "approve" ? "Review: approved" : "Review: needs changes"));
  box.append(rich("p", event.summary));
  if (event.must_fix && event.must_fix.length) {
    const findings = el("div", "findings");
    // One group per task, each with a button that sends it back to that task's worker.
    const tasks = [...new Set(event.must_fix.map((finding) => finding.task))].sort();
    for (const task of tasks) {
      const group = el("div", "finding-group");
      group.append(el("div", "goes-to", "Must fix in task " + task));
      const list = el("ul");
      for (const finding of event.must_fix.filter((f) => f.task === task)) list.append(rich("li", finding.issue));
      const sendBack = button("Send back to Worker " + task, () => {
        clearProblem();
        invoke("send_back", { runId, worker: task }).catch(showProblem);
      });
      sendBack.dataset.needs = "round";
      group.append(list, sendBack);
      findings.append(group);
    }
    box.append(findings);
  }
  if (event.consider && event.consider.length) {
    box.append(el("div", "goes-to", "Worth considering"));
    const list = el("ul");
    for (const item of event.consider) list.append(rich("li", item));
    box.append(list);
  }
  addToThread(box);

  // The reviewer's card gets a one-line version.
  addToTurn(agent, el("div", "verdict-line " + event.verdict,
    event.verdict === "approve" ? "Approved. Details in the conversation." : "Needs changes. Details in the conversation."));
  refreshControls();
  reveal(box);
}

// Decisions wait for you, so scroll the conversation to the top of a new one.
function reveal(box) {
  thread().scrollTop = box.offsetTop - 12;
}

// --- Usage ------------------------------------------------------------------------------

// 45210 -> "45.2k"
function count(tokens) {
  if (tokens < 1000) return String(tokens);
  if (tokens < 1000000) return (tokens / 1000).toFixed(1) + "k";
  return (tokens / 1000000).toFixed(2) + "M";
}

// One small line per finished session, and a running total for the whole run in the top bar.
function showUsage(agent, event) {
  const t = event.tokens;
  const total = t.input_tokens + t.output_tokens + t.cache_read_input_tokens + t.cache_creation_input_tokens;
  let line = count(total) + " tokens, " + count(t.cache_read_input_tokens) + " from cache";
  if (event.turns != null) line += ", " + event.turns + (event.turns === 1 ? " turn" : " turns");
  if (event.cost_usd != null) line += ", $" + event.cost_usd.toFixed(2);
  const node = el("div", "tokens", line);
  node.title = "New input " + count(t.input_tokens + t.cache_creation_input_tokens) + ", output " + count(t.output_tokens)
    + ". Cache reads are the cheap kind: the part of the prompt that was the same as the turn before. Dollars are API prices.";
  addToTurn(agent, node);

  runTokens += total;
  runCost += event.cost_usd || 0;
  const usage = $("run-usage");
  usage.hidden = false;
  usage.textContent = "This run: " + count(runTokens) + " tokens" + (runCost > 0 ? ", $" + runCost.toFixed(2) : "");
  usage.title = "API-equivalent cost. On a Claude plan login this is a measure of how much of your limits the run used.";
}

// --- Events from the Rust side ------------------------------------------------------------

function handle(event) {
  if (event.type === "run_state") return setRunState(event.state);
  if (event.type === "round") return setRound(event.open);
  const agent = agents[event.agent];
  if (!agent) {
    if (event.type === "error") addToThread(el("div", "problem", event.message));
    if (event.type === "notice") addToThread(el("div", "note", event.text));
    return;
  }
  switch (event.type) {
    case "agent_status":
      setStatus(agent, event.status);
      if (event.status === "running") {
        if (agent.turn) endTurn(agent, "done");
        startTurn(agent, true);
        if (agent.card) agent.card.model.textContent = modelLabel(agent.role);
      } else {
        endTurn(agent, event.status);
      }
      refreshControls();
      break;
    case "agent_model":
      if (agent.card) agent.card.model.textContent = event.model;
      if (agent.turn) agent.turn.root.querySelector(".model").textContent = event.model;
      break;
    case "user_message":
      youSaid(agent, event.text);
      break;
    case "text": addToTurn(agent, rich("div", event.text, "say")); break;
    case "tool": {
      const node = el("li", "step");
      node.append(el("b", "", event.name), " " + event.detail);
      addStep(agent, node, event.name + " " + event.detail);
      break;
    }
    case "tool_error":
      addStep(agent, el("li", "step failed", "Refused or failed: " + event.detail), "Refused or failed: " + event.detail);
      break;
    case "notice": place(agent, el("div", "note", event.text)); break;
    case "error": place(agent, el("div", "problem", event.message)); break;
    case "result": showUsage(agent, event); break;
    case "plan": showPlan(event); break;
    case "review": showReview(agent, event); break;
  }
}

// Where a notice or an error goes. What the app says for the reviewer while it is not
// running is about the whole round (merging, applying, discarding), so it goes in the
// conversation, where you are deciding.
function place(agent, node) {
  if (agent.turn) addToTurn(agent, node);
  else if (isChat(agent) || agent.role === "reviewer") addToThread(node);
  else addToCard(agent, node);
}

// Every event has a sequence number. Showing them strictly in order, each exactly
// once, means it does not matter whether an event arrives live or through catchUp().
function accept(event) {
  if (event.seq !== nextSeq) return false;
  nextSeq += 1;
  handle(event);
  return true;
}

// Fetch the run's full event list and show whatever we have not shown yet.
async function catchUp() {
  const events = await invoke("run_events", { runId });
  for (const event of events) accept(event);
}

function resetView() {
  thread().replaceChildren();
  clearCards();
  roundOpen = false;
  verdict = null;
  lastStage = 0;
  runCost = 0;
  runTokens = 0;
  $("run-usage").hidden = true;
}

function show(id) {
  runId = id;
  nextSeq = 0;
  runState = null;
  remember("agent-console-run", id);
  resetView();
  return catchUp();
}

function forgetRun() {
  runId = null;
  forget("agent-console-run");
  resetView();
  setRunState(null);
  refreshWelcome();
}

// --- Wiring up the page ---------------------------------------------------------------------

$("composer").addEventListener("submit", (submitted) => { submitted.preventDefault(); sendMain(); });
$("message").addEventListener("keydown", (key) => {
  if (key.key === "Enter" && (key.metaKey || key.ctrlKey)) { key.preventDefault(); sendMain(); }
});
for (const radio of document.querySelectorAll("input[name=mode]")) radio.addEventListener("change", refreshPlaceholder);

$("stop").addEventListener("click", () => {
  if (runId) invoke("stop_run", { runId }).catch(showProblem);
});

$("new-run").addEventListener("click", () => {
  if (roundOpen) return showProblem("Apply or discard the current changes first.");
  clearProblem();
  // The next message starts a fresh run with new agent sessions.
  forgetRun();
});

$("project").addEventListener("click", chooseFolder);
$("apply").addEventListener("click", () => roundAction("apply_changes"));
$("review-again").addEventListener("click", () => roundAction("review_again"));
$("discard").addEventListener("click", () => roundAction("discard_changes"));

// --- Settings window ------------------------------------------------------------------------

const SETTINGS_ROWS = [
  ["dispatcher", "Dispatcher", "plan-hue"],
  ["planner", "Planner", "plan-hue"],
  ["worker", "Workers", "build-hue"],
  ["reviewer", "Reviewer", "review-hue"],
];
let limits = null; // defaults and ranges from the Rust side, for placeholders and checks

function buildSettingsForm(presets) {
  for (const { preset, settings: values } of presets) {
    const [label, description] = PRESET_WORDS[preset] || [preset, ""];
    const presetButton = button(label, () => fillModelSection(values));
    presetButton.title = description;
    $("presets").append(presetButton);
  }
  const grid = $("settings-roles");
  for (const [key, label, hue] of SETTINGS_ROWS) {
    grid.append(el("span", "role " + hue, label));

    const pick = el("div", "model-pick");
    const choice = el("select");
    choice.id = "model-" + key;
    choice.setAttribute("aria-label", label + " model");
    for (const [value, text] of MODEL_CHOICES) choice.append(new Option(text, value));
    choice.append(new Option("Other", "other"));
    const custom = el("input");
    custom.type = "text";
    custom.id = "model-" + key + "-custom";
    custom.placeholder = "claude-opus-5-5";
    custom.spellcheck = false;
    custom.setAttribute("aria-label", label + " model ID");
    choice.addEventListener("change", () => { custom.hidden = choice.value !== "other"; if (!custom.hidden) custom.focus(); });
    pick.append(choice, custom);
    grid.append(pick);

    const effort = el("select");
    effort.id = "effort-" + key;
    effort.setAttribute("aria-label", label + " effort");
    for (const [value, text] of EFFORT_CHOICES) effort.append(new Option(text, value));
    grid.append(effort);

    // Empty means the role's default, which the placeholder shows.
    const turns = el("input");
    turns.type = "number";
    turns.id = "turns-" + key;
    turns.min = 1;
    turns.max = limits.max_turns;
    turns.step = 1;
    turns.placeholder = String(limits[key + "_turns"]);
    turns.setAttribute("aria-label", label + " turn limit, empty for the default of " + limits[key + "_turns"]);
    grid.append(turns);
  }
  $("chat-turns").max = limits.max_turns;
  $("chat-turns").placeholder = String(limits.chat_turns);
  for (let count = 1; count <= limits.max_workers; count++) {
    $("worker-count").append(new Option(count === 1 ? "1 worker" : count + " workers", String(count)));
  }
  $("notes").maxLength = limits.max_notes;
  $("notes").addEventListener("input", countNotes);
}

function countNotes() {
  const used = $("notes").value.length;
  $("notes-count").textContent = used + " of " + limits.max_notes + " characters. They are sent with every turn of every agent, mostly from the cache, so shorter is cheaper.";
}

const turnsText = (value) => (value == null ? "" : String(value));
function readTurns(id) {
  const text = $(id).value.trim();
  return text === "" ? null : Number(text);
}

// The part a preset changes: models, effort, turn limits and the spending cap.
function fillModelSection(values) {
  for (const [key] of SETTINGS_ROWS) {
    const model = values[key].model;
    const known = MODEL_CHOICES.some(([value]) => value === model);
    $("model-" + key).value = known ? model : "other";
    $("model-" + key + "-custom").value = known ? "" : model;
    $("model-" + key + "-custom").hidden = known;
    $("effort-" + key).value = values[key].effort || "";
    $("turns-" + key).value = turnsText(values[key].max_turns);
  }
  $("chat-turns").value = turnsText(values.chat_max_turns);
  $("budget").value = values.max_budget_usd ?? "";
  $("settings-notice").textContent = "";
}

function fillSettingsForm(values) {
  fillModelSection(values);
  $("commands").value = values.allowed_commands.join("\n");
  $("web-access").checked = values.web_access;
  $("use-mcp").checked = values.use_mcp_servers;
  $("worker-count").value = String(values.worker_count);
  $("notes").value = values.notes;
  countNotes();
}

function readSettingsForm() {
  const values = {};
  for (const [key] of SETTINGS_ROWS) {
    const choice = $("model-" + key).value;
    values[key] = {
      model: choice === "other" ? $("model-" + key + "-custom").value.trim() : choice,
      effort: $("effort-" + key).value || null,
      max_turns: readTurns("turns-" + key),
    };
  }
  const budget = $("budget").value.trim();
  values.max_budget_usd = budget === "" ? null : Number(budget);
  values.chat_max_turns = readTurns("chat-turns");
  values.use_mcp_servers = $("use-mcp").checked;
  values.web_access = $("web-access").checked;
  // The Rust side trims each line and drops empty ones and repeats.
  values.allowed_commands = $("commands").value.split("\n");
  values.worker_count = Number($("worker-count").value);
  values.notes = $("notes").value;
  return values;
}

$("open-settings").addEventListener("click", () => { fillSettingsForm(settings); $("settings").showModal(); });
$("settings-cancel").addEventListener("click", () => $("settings").close());
$("settings-reset").addEventListener("click", () => fillSettingsForm(defaults));
$("settings-form").addEventListener("submit", async (submitted) => {
  submitted.preventDefault();
  try {
    settings = await invoke("save_settings", { settings: readSettingsForm() });
    $("settings").close();
    refreshModelLabels();
    refreshTeam();
  } catch (error) { $("settings-notice").textContent = problemText(error); }
});

// --- Start-up ---------------------------------------------------------------------------------

(async function boot() {
  const info = await invoke("app_info");
  settings = info.settings;
  defaults = info.default_settings;
  limits = info.limits;
  $("auth-chip").textContent = AUTH_WORDS[info.auth] || info.auth;
  buildSettingsForm(info.presets);
  buildCards(info.agents);
  setFolder(recall("agent-console-folder") || "");
  resetView();
  setRunState(null);
  refreshWelcome();

  await listen("run-event", ({ payload }) => {
    if (payload.run_id !== runId) return;
    // A gap means we missed something (for example while the page was loading): refetch.
    if (!accept(payload.event) && payload.event.seq > nextSeq) catchUp().catch(showProblem);
  });

  // If the window was reloaded mid-run, pick the run back up and replay its events.
  const previous = recall("agent-console-run");
  if (previous) {
    runFolder = recall("agent-console-run-folder");
    show(previous).catch(forgetRun);
  }
})();
