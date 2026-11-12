// The page. It talks to the Rust side in two ways:
//   invoke("command_name", {args})  calls a #[tauri::command] function and awaits its result
//   listen("run-event", handler)    receives events the Rust side emits while agents work
// Both come from window.__TAURI__, which Tauri injects (see "withGlobalTauri" in tauri.conf.json).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STATUS_WORDS = { idle: "Waiting", running: "Working", done: "Finished", error: "Failed", stopped: "Stopped" };
const STATE_WORDS = {
  ready: "Ready: message the team",
  chatting: "An agent is replying",
  planning: "Planning",
  awaiting_approval: "Waiting for you to approve the plan",
  building: "Workers are building",
  reviewing: "The reviewer is checking",
  done: "Reviewed: apply, discard, or send work back",
  failed: "Stopped on an error",
  stopped: "Stopped",
};
const EMPTY_TEXT = {
  jarvis: "Messages from the box above come here first. Jarvis answers quick questions itself and passes the rest to the planner.",
  planner: "Talk ideas through here, then press Make plan.",
  worker: "Starts when you approve a plan, in its own copy of the project. Afterwards you can message it here.",
  reviewer: "Checks all the workers' changes together when they finish. Then you apply or discard them.",
};
const PLACEHOLDER = {
  planner: "Message the planner",
  worker: "Ask this worker for a change",
  reviewer: "Ask the reviewer about its verdict",
};
const HUE = { jarvis: "plan-hue", planner: "plan-hue", worker: "build-hue", reviewer: "review-hue" };
const ROLE_SETTINGS = { jarvis: "jarvis", planner: "planner", worker: "worker", reviewer: "reviewer" };
const BUSY_STATES = ["chatting", "planning", "building", "reviewing"];
const REST_STATES = ["ready", "awaiting_approval", "done", "stopped", "failed"];
const AUTH_WORDS = { claude_login: "Signed in through Claude Code", api_key: "Billing: API key" };
const MODEL_CHOICES = [["opus", "Opus"], ["sonnet", "Sonnet"], ["haiku", "Haiku"]];
const PRESET_WORDS = {
  lean: ["Lean", "Least usage: Sonnet plans, low effort for workers and reviewer"],
  balanced: ["Balanced", "The default: Opus plans, Sonnet builds and reviews, medium effort"],
  thorough: ["Thorough", "Most careful and most usage: high effort, Opus also reviews"],
};
const EFFORT_CHOICES = [["", "Default"], ["low", "Low"], ["medium", "Medium"], ["high", "High"], ["xhigh", "Extra high"], ["max", "Max"]];

const $ = (id) => document.getElementById(id);
const panels = {};     // agent id -> { root, log, role, number, composer, input, hasWorked }
let settings = null;   // the current settings, as the Rust side last confirmed them
let defaults = null;   // the built-in defaults, for "Reset to defaults"
let runId = null;      // the run this window is showing
let runFolder = null;  // the project folder and demo switch the run was started with
let runDemo = null;
let runState = null;
let roundOpen = false; // true while a round's changes wait to be applied or discarded
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

// Commands fail with a plain string (Rust's Err(String)).
function problemText(error) { return typeof error === "string" ? error : String(error.message || error); }
function showProblem(error) { $("notice").textContent = problemText(error); }

const isBusy = () => BUSY_STATES.includes(runState);
const isAtRest = () => runId !== null && REST_STATES.includes(runState);

// "opus" plus "high" -> "opus, high effort"
function modelLabel(role) {
  const chosen = settings[ROLE_SETTINGS[role]];
  return chosen.effort ? chosen.model + ", " + chosen.effort + " effort" : chosen.model;
}

// --- Building the panels -------------------------------------------------------

function buildPanels(agents) {
  for (const agent of agents) {
    const root = el("section", "agent " + HUE[agent.role]);
    root.dataset.agent = agent.id;
    const header = el("header");
    header.append(el("h2", "", agent.label), el("span", "chip model"), el("span", "status"));
    const log = el("div", "log");
    root.append(header, log);
    const panel = { root, log, role: agent.role, number: Number(agent.id.split("-")[1]) || 0, hasWorked: false };

    // Jarvis is reached through the main message box; everyone else has their own.
    if (agent.role !== "jarvis") {
      const composer = el("form", "composer");
      const input = el("textarea");
      input.rows = 1;
      input.placeholder = PLACEHOLDER[agent.role];
      input.setAttribute("aria-label", PLACEHOLDER[agent.role]);
      const send = el("button", "", "Send");
      send.type = "submit";
      composer.append(input, send);
      if (agent.role === "planner") {
        const makePlan = button("Make plan", () => requestPlan(input), "make-plan");
        makePlan.title = "Turn the conversation into tasks for the workers. Anything typed here is passed along.";
        composer.append(makePlan);
      }
      composer.addEventListener("submit", (submitted) => { submitted.preventDefault(); sendToAgent(agent.id, input); });
      input.addEventListener("keydown", (key) => {
        if (key.key === "Enter" && (key.metaKey || key.ctrlKey)) { key.preventDefault(); composer.requestSubmit(); }
      });
      panel.composer = composer;
      panel.input = input;
    }
    if (agent.role === "reviewer") root.append(buildRoundActions());
    if (panel.composer) root.append(panel.composer);

    $("lanes").append(root);
    panels[agent.id] = panel;
  }
}

// The buttons for the open round. They live in the reviewer's panel because they
// act on what the reviewer has just checked.
function buildRoundActions() {
  const bar = el("div", "round-actions");
  bar.id = "round-actions";
  bar.hidden = true;
  bar.append(
    button("Apply to project", () => roundAction("apply_changes"), "apply"),
    button("Review again", () => roundAction("review_again"), "review-again"),
    button("Discard", () => roundAction("discard_changes"), "discard"),
  );
  bar.querySelector(".review-again").title = "After talking to a worker: check everything again";
  bar.querySelector(".discard").title = "Delete the workers' copies; your project is not touched";
  bar.querySelector(".apply").title = "Put the changes into your project as staged changes, for you to commit";
  return bar;
}

function resetPanels() {
  for (const panel of Object.values(panels)) {
    panel.log.replaceChildren(el("p", "empty", EMPTY_TEXT[panel.role]));
    panel.hasWorked = false;
    setStatus(panel, "idle");
  }
  setRound(false);
  refreshModelChips();
  refreshControls();
}

// Panels that have not worked yet show the model from the settings. Once an agent
// starts, its chip shows the exact model it is running on.
function refreshModelChips() {
  for (const panel of Object.values(panels)) {
    if (panel.root.dataset.status === "idle") panel.root.querySelector(".model").textContent = modelLabel(panel.role);
  }
}

function setStatus(panel, status) {
  panel.root.dataset.status = status;
  panel.root.querySelector(".status").textContent = STATUS_WORDS[status] || status;
  // "error" can mean it stopped at a turn limit or the cap; its session is kept, so it
  // can be told to continue. (If it never started, the Rust side says so.)
  if (status === "done" || status === "error") panel.hasWorked = true;
}

function setRound(open) {
  roundOpen = open;
  if (!open) {
    // The workers' and reviewer's sessions ended with the round.
    for (const panel of Object.values(panels)) {
      if (panel.role === "worker" || panel.role === "reviewer") panel.hasWorked = false;
    }
    for (const old of document.querySelectorAll(".verdict button")) old.remove();
  }
  refreshControls();
}

// Enable exactly what can be done right now.
function refreshControls() {
  const busy = isBusy();
  const rest = isAtRest();
  $("stop").disabled = !busy;
  for (const id of ["send", "send-brainstorm", "send-plan"]) $(id).disabled = busy;
  $("demo").disabled = busy;
  $("new-run").disabled = busy || runId === null;
  for (const panel of Object.values(panels)) {
    if (!panel.composer) continue;
    // Workers and the reviewer can be messaged during the round they worked in.
    const usable = rest && (panel.role === "planner" || (panel.hasWorked && roundOpen));
    for (const control of panel.composer.querySelectorAll("textarea, button")) control.disabled = !usable;
  }
  const actions = $("round-actions");
  actions.hidden = !roundOpen;
  for (const control of actions.querySelectorAll("button")) control.disabled = !rest;
  for (const node of document.querySelectorAll("[data-needs]")) {
    node.disabled = node.dataset.needs === "approval" ? runState !== "awaiting_approval" || roundOpen : !(rest && roundOpen);
  }
}

function add(panel, node) {
  const empty = panel.log.querySelector(".empty");
  if (empty) empty.remove();
  const nearBottom = panel.log.scrollHeight - panel.log.scrollTop - panel.log.clientHeight < 80;
  panel.log.append(node);
  if (nearBottom) panel.log.scrollTop = panel.log.scrollHeight;
}

function setRunState(state) {
  runState = state;
  $("run-state").innerHTML = state ? "Run status: <strong>" + STATE_WORDS[state] + "</strong>" : "Nothing running";
  if (state === "building") lockPlans(); // the plan was approved
  refreshControls();
}

// --- Sending ---------------------------------------------------------------------

// The main message box. A message continues the current run if it is for the same
// folder and mode; otherwise it starts a new run.
async function sendMain(mode) {
  const message = $("request").value.trim();
  if (!message) return;
  $("notice").textContent = "";
  const folder = $("project-dir").value;
  const demo = $("demo").checked;
  remember("agent-console-folder", folder);
  try {
    if (runId !== null && folder === runFolder && demo === runDemo) {
      await invoke("send", { runId, message, mode });
    } else {
      const id = await invoke("start_run", { projectDir: folder, message, mode, demo });
      runFolder = folder;
      runDemo = demo;
      remember("agent-console-run-folder", folder);
      remember("agent-console-run-demo", demo ? "1" : "0");
      await show(id);
    }
    $("request").value = "";
  } catch (error) { showProblem(error); }
}

async function sendToAgent(agent, input) {
  const message = input.value.trim();
  if (!message || !runId) return;
  $("notice").textContent = "";
  try {
    await invoke("send_message", { runId, agent, message });
    input.value = "";
  } catch (error) { showProblem(error); }
}

async function requestPlan(input) {
  if (!runId) return;
  $("notice").textContent = "";
  try {
    await invoke("make_plan", { runId, guidance: input.value.trim() || null });
    input.value = "";
  } catch (error) { showProblem(error); }
}

function roundAction(command) {
  $("notice").textContent = "";
  invoke(command, { runId }).catch(showProblem);
}

// --- The plan and the verdict ---------------------------------------------------

function lockPlans() {
  for (const plan of document.querySelectorAll(".plan")) {
    for (const node of plan.querySelectorAll("button")) node.remove();
    for (const field of plan.querySelectorAll("input, textarea")) field.readOnly = true;
  }
}

function showPlan(panel, event) {
  // A newer plan replaces any earlier one that was never approved.
  for (const older of document.querySelectorAll(".plan")) older.classList.add("superseded");
  lockPlans();

  const box = el("div", "plan");
  box.append(el("p", "", event.summary));
  event.tasks.forEach((task, index) => {
    const card = el("div", "task");
    card.append(el("div", "goes-to", "Task " + (index + 1) + ", for Worker " + (index + 1)));
    const title = el("input"); title.type = "text"; title.value = task.title; title.dataset.field = "title";
    title.setAttribute("aria-label", "Task " + (index + 1) + " title");
    const instructions = el("textarea"); instructions.value = task.instructions; instructions.dataset.field = "instructions";
    instructions.setAttribute("aria-label", "Task " + (index + 1) + " instructions");
    const files = el("input", "files"); files.type = "text"; files.value = task.files.join(", "); files.dataset.field = "files";
    files.setAttribute("aria-label", "Files for task " + (index + 1) + ", separated by commas");
    card.append(title, instructions, files);
    box.append(card);
  });
  const approve = button("Approve plan and start workers", async () => {
    const tasks = [...box.querySelectorAll(".task")].map((card) => ({
      title: card.querySelector("[data-field=title]").value,
      instructions: card.querySelector("[data-field=instructions]").value,
      files: card.querySelector("[data-field=files]").value.split(",").map((f) => f.trim()).filter(Boolean),
    }));
    try { await invoke("approve_plan", { runId, tasks }); } catch (error) { showProblem(error); }
  }, "primary");
  approve.dataset.needs = "approval";
  box.append(approve);
  box.append(el("p", "hint", "Not quite right? Tell the planner below what to change, then press Make plan again."));
  add(panel, box);
  refreshControls();
}

function showReview(panel, event) {
  for (const old of panel.log.querySelectorAll(".verdict button")) old.remove();
  const box = el("div", "verdict " + event.verdict);
  box.append(el("h3", "", event.verdict === "approve" ? "Approved" : "Needs changes"));
  box.append(el("p", "", event.summary));
  if (event.must_fix && event.must_fix.length) {
    box.append(el("div", "kind", "Must fix"));
    // One group per task, each with a button that sends it back to that task's worker.
    const tasks = [...new Set(event.must_fix.map((finding) => finding.task))].sort();
    for (const task of tasks) {
      const group = el("div", "task-group");
      group.append(el("div", "goes-to", "Task " + task));
      const list = el("ul");
      for (const finding of event.must_fix.filter((f) => f.task === task)) list.append(el("li", "", finding.issue));
      const sendBack = button("Send back to Worker " + task, () => {
        invoke("send_back", { runId, worker: task }).catch(showProblem);
      });
      sendBack.dataset.needs = "round";
      group.append(list, sendBack);
      box.append(group);
    }
  }
  if (event.consider && event.consider.length) {
    box.append(el("div", "kind", "Worth considering"));
    const list = el("ul");
    for (const item of event.consider) list.append(el("li", "", item));
    box.append(list);
  }
  add(panel, box);
  refreshControls();
}

// --- Usage ------------------------------------------------------------------------

// 45210 -> "45.2k"
function count(tokens) {
  if (tokens < 1000) return String(tokens);
  if (tokens < 1000000) return (tokens / 1000).toFixed(1) + "k";
  return (tokens / 1000000).toFixed(2) + "M";
}

// One line per finished session, and a running total for the whole run in the top bar.
function showUsage(panel, event) {
  const t = event.tokens;
  const total = t.input_tokens + t.output_tokens + t.cache_read_input_tokens + t.cache_creation_input_tokens;
  const parts = [
    count(t.cache_read_input_tokens) + " read from cache",
    count(t.input_tokens + t.cache_creation_input_tokens) + " new input",
    count(t.output_tokens) + " output",
  ];
  let line = "Tokens: " + parts.join(", ") + ". " + (event.turns ?? "?") + " turns";
  if (event.cost_usd != null) line += ", worth $" + event.cost_usd.toFixed(2) + " at API prices";
  const node = el("div", "cost", line + ".");
  node.title = "Cache reads are the cheap kind: the part of the prompt that was the same as the turn before.";
  add(panel, node);

  runTokens += total;
  runCost += event.cost_usd || 0;
  const usage = $("run-usage");
  usage.hidden = false;
  usage.textContent = "This run: " + count(runTokens) + " tokens" + (runCost > 0 ? ", $" + runCost.toFixed(2) : "");
  usage.title = "API-equivalent cost. On a Claude plan login this is a measure of how much of your limits the run used.";
}

// --- Events from the Rust side ---------------------------------------------------

function handle(event) {
  if (event.type === "run_state") return setRunState(event.state);
  if (event.type === "round") return setRound(event.open);
  const panel = panels[event.agent];
  if (!panel) {
    if (event.type === "error") showProblem(event.message);
    return;
  }
  switch (event.type) {
    case "agent_status":
      setStatus(panel, event.status);
      if (event.status === "running") panel.root.querySelector(".model").textContent = modelLabel(panel.role);
      refreshControls();
      break;
    case "agent_model":
      panel.root.querySelector(".model").textContent = event.model;
      break;
    case "user_message":
      add(panel, el("div", "you", event.text));
      break;
    case "text": {
      const node = el("div", "say");
      node.innerHTML = richText(event.text);
      add(panel, node);
      break;
    }
    case "tool": {
      const node = el("div", "tool");
      node.append(el("b", "", event.name), " " + event.detail);
      add(panel, node);
      break;
    }
    case "tool_error":
      add(panel, el("div", "tool failed", "Refused or failed: " + event.detail));
      break;
    case "notice":
      add(panel, el("div", "app-note", event.text));
      break;
    case "error":
      add(panel, el("div", "problem", event.message));
      break;
    case "result":
      showUsage(panel, event);
      break;
    case "plan": showPlan(panel, event); break;
    case "review": showReview(panel, event); break;
  }
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

function show(id) {
  runId = id;
  nextSeq = 0;
  runState = null;
  runCost = 0;
  runTokens = 0;
  $("run-usage").hidden = true;
  remember("agent-console-run", id);
  resetPanels();
  return catchUp();
}

// --- Wiring up the page ------------------------------------------------------------

$("brief").addEventListener("submit", (submitted) => { submitted.preventDefault(); sendMain("jarvis"); });
$("send-brainstorm").addEventListener("click", () => sendMain("brainstorm"));
$("send-plan").addEventListener("click", () => sendMain("plan"));
$("request").addEventListener("keydown", (key) => {
  if (key.key === "Enter" && (key.metaKey || key.ctrlKey)) { key.preventDefault(); sendMain("jarvis"); }
});

$("stop").addEventListener("click", () => {
  if (runId) invoke("stop_run", { runId }).catch(showProblem);
});

$("new-run").addEventListener("click", () => {
  if (roundOpen) return showProblem("Apply or discard the current changes first.");
  // The next message starts a fresh run with new agent sessions.
  runId = null;
  runState = null;
  forget("agent-console-run");
  resetPanels();
  setRunState(null);
  $("run-usage").hidden = true;
});

$("choose-folder").addEventListener("click", async () => {
  const folder = await invoke("pick_folder").catch(() => null);
  if (folder) $("project-dir").value = folder;
});

$("demo").addEventListener("change", () => remember("agent-console-demo", $("demo").checked ? "1" : "0"));

// --- Settings window ---------------------------------------------------------------

const SETTINGS_ROWS = [
  ["jarvis", "Jarvis", "plan-hue"],
  ["planner", "Planner", "plan-hue"],
  ["worker", "Workers", "build-hue"],
  ["reviewer", "Reviewer", "review-hue"],
];

function buildSettingsForm(presets) {
  for (const { preset, settings: values } of presets) {
    const [label, description] = PRESET_WORDS[preset] || [preset, ""];
    const presetButton = button(label, () => fillSettingsForm(values));
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
  }
}

function fillSettingsForm(values) {
  for (const [key] of SETTINGS_ROWS) {
    const model = values[key].model;
    const known = MODEL_CHOICES.some(([value]) => value === model);
    $("model-" + key).value = known ? model : "other";
    $("model-" + key + "-custom").value = known ? "" : model;
    $("model-" + key + "-custom").hidden = known;
    $("effort-" + key).value = values[key].effort || "";
  }
  $("budget").value = values.max_budget_usd ?? "";
  $("use-mcp").checked = values.use_mcp_servers;
  $("settings-notice").textContent = "";
}

function readSettingsForm() {
  const values = {};
  for (const [key] of SETTINGS_ROWS) {
    const choice = $("model-" + key).value;
    values[key] = {
      model: choice === "other" ? $("model-" + key + "-custom").value.trim() : choice,
      effort: $("effort-" + key).value || null,
    };
  }
  const budget = $("budget").value.trim();
  values.max_budget_usd = budget === "" ? null : Number(budget);
  values.use_mcp_servers = $("use-mcp").checked;
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
    refreshModelChips();
  } catch (error) { $("settings-notice").textContent = problemText(error); }
});

// --- Start-up ----------------------------------------------------------------------

(async function boot() {
  const info = await invoke("app_info");
  settings = info.settings;
  defaults = info.default_settings;
  $("auth-chip").textContent = AUTH_WORDS[info.auth] || info.auth;
  buildSettingsForm(info.presets);
  buildPanels(info.agents);
  resetPanels();
  setRunState(null);
  $("project-dir").value = recall("agent-console-folder") || "";
  $("demo").checked = info.demo_default || recall("agent-console-demo") === "1";

  await listen("run-event", ({ payload }) => {
    if (payload.run_id !== runId) return;
    // A gap means we missed something (for example while the page was loading): refetch.
    if (!accept(payload.event) && payload.event.seq > nextSeq) catchUp().catch(showProblem);
  });

  // If the window was reloaded mid-run, pick the run back up and replay its events.
  const previous = recall("agent-console-run");
  if (previous) {
    runFolder = recall("agent-console-run-folder");
    runDemo = recall("agent-console-run-demo") === "1";
    show(previous).catch(() => { forget("agent-console-run"); runId = null; runState = null; resetPanels(); setRunState(null); });
  }
})();
