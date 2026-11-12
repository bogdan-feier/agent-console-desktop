// The page. It talks to the Rust side in two ways:
//   invoke("command_name", {args})  calls a #[tauri::command] function and awaits its result
//   listen("run-event", handler)    receives events the Rust side emits while agents work
// Both come from window.__TAURI__, which Tauri injects (see "withGlobalTauri" in tauri.conf.json).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STATUS_WORDS = { idle: "Waiting", running: "Working", done: "Finished", error: "Failed", stopped: "Stopped" };
const STATE_WORDS = {
  planning: "Planning",
  awaiting_approval: "Waiting for you to approve the plan",
  building: "Workers are building",
  reviewing: "Reviewers are checking",
  done: "Finished",
  failed: "Stopped on an error",
  stopped: "Stopped",
};
const EMPTY_TEXT = {
  planner: "The plan appears here after you start.",
  worker: "Starts when you approve the plan.",
  reviewer: "Starts when its worker finishes.",
};
const HUE = { planner: "plan-hue", worker: "build-hue", reviewer: "review-hue" };
const BUSY_STATES = ["planning", "building", "reviewing"];
const AUTH_WORDS = { claude_login: "Signed in through Claude Code", api_key: "Billing: API key" };

const $ = (id) => document.getElementById(id);
const panels = {};   // agent id -> { root, log, role, number }
let runId = null;    // the run this window is showing
let runState = null;
let nextSeq = 0;     // sequence number of the next event we expect for this run

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

// Escape first, then allow `code` and **bold**, so agent text can never inject HTML.
function richText(text) {
  const escaped = text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  return escaped.replace(/`([^`\n]+)`/g, "<code>$1</code>").replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
}

// Commands fail with a plain string (Rust's Err(String)); show it under the form.
function showProblem(error) { $("notice").textContent = typeof error === "string" ? error : String(error.message || error); }

// --- Building the panels -------------------------------------------------------

function buildPanels(agents) {
  for (const agent of agents) {
    const root = el("section", "agent " + HUE[agent.role]);
    root.dataset.agent = agent.id;
    const header = el("header");
    header.append(el("h2", "", agent.label), el("span", "chip model", agent.model), el("span", "status"));
    const log = el("div", "log");
    root.append(header, log);
    $("lanes").append(root);
    panels[agent.id] = { root, log, role: agent.role, number: Number(agent.id.split("-")[1]) || 0, model: agent.model };
  }
}

function resetPanels() {
  for (const panel of Object.values(panels)) {
    panel.log.replaceChildren(el("p", "empty", EMPTY_TEXT[panel.role]));
    panel.root.querySelector(".model").textContent = panel.model;
    setStatus(panel, "idle");
  }
}

function setStatus(panel, status) {
  panel.root.dataset.status = status;
  panel.root.querySelector(".status").textContent = STATUS_WORDS[status] || status;
}

function add(panel, node) {
  const empty = panel.log.querySelector(".empty");
  if (empty) empty.remove();
  const nearBottom = panel.log.scrollHeight - panel.log.scrollTop - panel.log.clientHeight < 60;
  panel.log.append(node);
  if (nearBottom) panel.log.scrollTop = panel.log.scrollHeight;
}

function setRunState(state) {
  runState = state;
  $("run-state").innerHTML = state ? "Run status: <strong>" + STATE_WORDS[state] + "</strong>" : "Nothing running";
  const busy = BUSY_STATES.includes(state);
  $("stop").disabled = !busy;
  $("start").disabled = busy;
  $("demo").disabled = busy;
  if (state && state !== "planning" && state !== "awaiting_approval") {
    // The plan has been approved (or the run ended): lock it.
    for (const button of document.querySelectorAll(".plan button")) button.remove();
    for (const field of document.querySelectorAll(".plan input, .plan textarea")) field.readOnly = true;
  }
  for (const button of document.querySelectorAll("[data-needs-state]")) {
    button.disabled = !button.dataset.needsState.split(" ").includes(state);
  }
}

// --- The plan and the verdicts ---------------------------------------------------

function showPlan(panel, event) {
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
  const approve = el("button", "primary", "Approve plan and start workers");
  approve.dataset.needsState = "awaiting_approval";
  approve.disabled = runState !== "awaiting_approval";
  approve.addEventListener("click", async () => {
    const tasks = [...box.querySelectorAll(".task")].map((card) => ({
      title: card.querySelector("[data-field=title]").value,
      instructions: card.querySelector("[data-field=instructions]").value,
      files: card.querySelector("[data-field=files]").value.split(",").map((f) => f.trim()).filter(Boolean),
    }));
    try { await invoke("approve_plan", { runId, tasks }); } catch (error) { showProblem(error); }
  });
  box.append(approve);
  add(panel, box);
}

function showReview(panel, event) {
  for (const old of panel.log.querySelectorAll(".verdict button")) old.remove();
  const box = el("div", "verdict " + event.verdict);
  box.append(el("h3", "", event.verdict === "approve" ? "Approved" : "Needs changes"));
  box.append(el("p", "", event.summary));
  for (const [title, items] of [["Must fix", event.must_fix], ["Worth considering", event.consider]]) {
    if (!items || !items.length) continue;
    box.append(el("div", "kind", title));
    const list = el("ul");
    for (const item of items) list.append(el("li", "", item));
    box.append(list);
  }
  if (event.verdict === "needs_changes") {
    const sendBack = el("button", "", "Send back to Worker " + panel.number);
    // Offered whenever the run is at rest, including after a fix round that was stopped.
    sendBack.dataset.needsState = "done stopped failed";
    sendBack.disabled = !sendBack.dataset.needsState.split(" ").includes(runState);
    sendBack.addEventListener("click", () => {
      invoke("send_back", { runId, worker: panel.number }).catch(showProblem);
    });
    box.append(sendBack);
  }
  add(panel, box);
}

// --- Events from the Rust side ---------------------------------------------------

function handle(event) {
  if (event.type === "run_state") return setRunState(event.state);
  const panel = panels[event.agent];
  if (!panel) {
    if (event.type === "error") showProblem(event.message);
    return;
  }
  switch (event.type) {
    case "agent_status":
      if (event.status === "running" && !panel.log.querySelector(".empty") && panel.log.children.length) {
        add(panel, el("div", "round", "Next round"));
      }
      setStatus(panel, event.status);
      break;
    case "agent_model":
      panel.root.querySelector(".model").textContent = event.model;
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
    case "error":
      add(panel, el("div", "problem", event.message));
      break;
    case "result":
      if (event.cost_usd != null) {
        add(panel, el("div", "cost", "Usage worth $" + event.cost_usd.toFixed(2) + " over " + (event.turns ?? "?") + " turns"));
      }
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
  remember("agent-console-run", id);
  resetPanels();
  return catchUp();
}

// --- Wiring up the page ------------------------------------------------------------

$("brief").addEventListener("submit", async (submitted) => {
  submitted.preventDefault();
  $("notice").textContent = "";
  remember("agent-console-folder", $("project-dir").value);
  remember("agent-console-request", $("request").value);
  try {
    const id = await invoke("start_run", {
      projectDir: $("project-dir").value,
      request: $("request").value,
      demo: $("demo").checked,
    });
    await show(id);
  } catch (error) { showProblem(error); }
});

$("stop").addEventListener("click", () => {
  if (runId) invoke("stop_run", { runId }).catch(showProblem);
});

$("choose-folder").addEventListener("click", async () => {
  const folder = await invoke("pick_folder").catch(() => null);
  if (folder) $("project-dir").value = folder;
});

$("demo").addEventListener("change", () => remember("agent-console-demo", $("demo").checked ? "1" : "0"));

(async function start() {
  const info = await invoke("app_info");
  $("auth-chip").textContent = AUTH_WORDS[info.auth] || info.auth;
  buildPanels(info.agents);
  resetPanels();
  setRunState(null);
  $("project-dir").value = recall("agent-console-folder") || "";
  $("request").value = recall("agent-console-request") || "";
  $("demo").checked = info.demo_default || recall("agent-console-demo") === "1";

  await listen("run-event", ({ payload }) => {
    if (payload.run_id !== runId) return;
    // A gap means we missed something (for example while the page was loading): refetch.
    if (!accept(payload.event) && payload.event.seq > nextSeq) catchUp().catch(showProblem);
  });

  // If the window was reloaded mid-run, pick the run back up and replay its events.
  const previous = recall("agent-console-run");
  if (previous) show(previous).catch(() => { forget("agent-console-run"); runId = null; });
})();
