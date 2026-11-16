// Agent Console: the page.
//
// The window has two halves. On the left, the conversation: you, the dispatcher
// and the planner, plus the cards where you decide things. On the right, the
// team: a card per worker, the reviewer, and the resolver when it is needed.
//
// This file holds the parts that tie the rest together: sending what you type,
// turning events from the Rust side into things on the screen, and start-up.
// Everything else lives in parts/ and is imported here.

import { buildCards, clearCards, setStatus } from "./parts/cards.js";
import { wireChanges } from "./parts/changes.js";
import {
  mode, refreshControls, refreshPlaceholder, refreshStages, setRound, setRunState,
} from "./parts/controls.js";
import { showChecks, showPlan, showReview } from "./parts/decisions.js";
import { $, button, clearProblem, el, forget, recall, remember, showProblem } from "./parts/dom.js";
import { wireHistory } from "./parts/history.js";
import { rich } from "./parts/markdown.js";
import { refreshPrompts, wirePrompts } from "./parts/prompts.js";
import { chooseFolder, refreshWelcome, setFolder } from "./parts/project.js";
import { allowCommand, buildSettingsForm, wireSettings } from "./parts/settings.js";
import { agents, app, isAtRest, modelLabel } from "./parts/state.js";
import { canNotify, invoke, listen } from "./parts/tauri.js";
import {
  addStep, addToThread, addToTurn, endTurn, place, showRunTotal, showUsage, startTurn, thread, youSaid,
} from "./parts/turns.js";
import { AUTH_WORDS, count, plural } from "./parts/words.js";

// --- Sending ----------------------------------------------------------------

/// The main message box. A message continues the current run if it is for the
/// same folder; otherwise it starts a new one.
async function sendMain() {
  const message = $("message").value.trim();
  const chosen = mode();
  clearProblem();
  if (!app.folder) return showProblem("Choose a project first.");
  try {
    if (app.runId !== null && !app.readOnly && app.folder === app.runFolder) {
      if (chosen === "plan") await invoke("make_plan", { runId: app.runId, guidance: message || null });
      else if (!message) return;
      else await invoke("send", { runId: app.runId, message, mode: chosen });
    } else {
      if (!message) return;
      const id = await invoke("start_run", { projectDir: app.folder, message, mode: chosen });
      // A run is the moment to ask: from here on the user may walk away, and
      // macOS only ever shows its permission box once.
      canNotify();
      app.runFolder = app.folder;
      remember("agent-console-run-folder", app.folder);
      await show(id);
    }
    $("message").value = "";
    refreshPrompts();
  } catch (error) {
    showProblem(error);
  }
}

async function sendToAgent(agent, input) {
  const message = input.value.trim();
  if (!message || !app.runId) return;
  clearProblem();
  try {
    await invoke("send_message", { runId: app.runId, agent: agent.id, message });
    input.value = "";
  } catch (error) {
    showProblem(error);
  }
}

function roundAction(command) {
  clearProblem();
  invoke(command, { runId: app.runId }).catch(showProblem);
}

// --- Events from the Rust side ----------------------------------------------

function handle(event) {
  if (event.type === "run_state") return setRunState(event.state);
  if (event.type === "round") return setRound(event.open, event.quick);
  if (event.type === "checks") return showChecks(event);

  const agent = agents[event.agent];
  if (!agent) {
    // Something about the run as a whole.
    if (event.type === "error") addToThread(problemBox(event));
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
    case "user_message": youSaid(agent, event.text); break;
    case "text": addToTurn(agent, rich("div", event.text, "say")); break;
    case "tool": {
      const node = el("li", "step");
      node.append(el("b", "", event.name), " " + event.detail);
      addStep(agent, node, event.name + " " + event.detail);
      break;
    }
    case "tool_error": addStep(agent, refusedStep(agent, event), "Refused or failed: " + event.detail); break;
    case "notice": place(agent, el("div", "note", event.text)); break;
    case "error": place(agent, problemBox(event)); break;
    case "result":
      showUsage(agent, event);
      watchSessionLength(agent, event);
      break;
    case "plan": showPlan(event); break;
    case "review": showReview(agent, event); break;
  }
}

/// Something went wrong. When the app can clear it up itself, offer that: a folder
/// with uncommitted changes in it is the app's job to put away, not an errand.
function problemBox(event) {
  const box = el("div", "problem", event.message);
  if (!event.tidy_with || app.readOnly) return box;

  const commit = button("Commit my changes and carry on", async () => {
    commit.disabled = true;
    try {
      const files = await invoke("tidy_project", {
        projectDir: app.runFolder || app.folder,
        message: event.tidy_with,
      });
      commit.replaceWith(el("span", "allowed",
        " Committed " + plural(files, "file") + ". Try again now."));
    } catch (error) {
      commit.replaceWith(el("span", "problem", " " + String(error)));
    }
  });
  box.append(el("p", "hint", 'Committed as "' + event.tidy_with + '", so nothing is lost.'));
  box.append(commit);
  return box;
}

/// A step that was refused. When it was a shell command, offer to allow it:
/// that is the fastest way to find out what a project needs.
function refusedStep(agent, event) {
  const node = el("li", "step failed", "Refused or failed: " + event.detail);
  if (!event.allow_rule) return node;

  const allow = button('Allow "' + event.allow_rule + '"', async () => {
    allow.disabled = true;
    try {
      await allowCommand(event.allow_rule);
      const canRetry = isAtRest() && agent.hasWorked && app.roundOpen;
      if (canRetry) {
        await invoke("send_message", {
          runId: app.runId,
          agent: agent.id,
          message: 'I have allowed `' + event.allow_rule + '`. Run it again and carry on.',
        });
      }
      allow.replaceWith(el("span", "allowed", canRetry
        ? " Allowed, and the agent was asked to try again."
        : " Allowed. Message the agent to try again."));
    } catch (error) {
      allow.replaceWith(el("span", "problem", " " + String(error)));
    }
  }, "allow");
  allow.title = "Add this to the allowed commands in Settings";
  node.append(" ", allow);
  return node;
}

/// How much of a session's prompt is the conversation so far, in tokens, past
/// which it is worth offering to start again.
const LONG_SESSION = 120000;
const warnedAbout = new Set();

/// The dispatcher and the planner keep their conversation for the whole run, so
/// it grows with every round and is resent on every turn. When one gets big,
/// say so once and offer the way out.
function watchSessionLength(agent, event) {
  const used = event.tokens;
  const prompt = used.input_tokens + used.cache_read_input_tokens + used.cache_creation_input_tokens;
  const keepsTalking = agent.role === "dispatcher" || agent.role === "planner";
  if (!keepsTalking || prompt < LONG_SESSION || warnedAbout.has(agent.id) || app.replaying) return;
  warnedAbout.add(agent.id);

  const note = el("div", "note");
  note.append(agent.label + "'s conversation is up to " + count(prompt)
    + " tokens, and all of it is sent again on every turn. ");
  const fresh = button("Start " + agent.label + " fresh", async () => {
    try {
      await invoke("fresh_session", { runId: app.runId, agent: agent.id });
      fresh.replaceWith(el("span", "", "Started fresh."));
    } catch (error) {
      fresh.replaceWith(el("span", "problem", String(error)));
    }
  });
  fresh.title = "Forget this agent's conversation so far. The rest of the team is unchanged.";
  note.append(fresh);
  // Inside the turn, not after it: what the agent said next belongs to the same
  // turn, and anything added between the two would split it in half.
  addToTurn(agent, note);
}

/// Every event has a sequence number. Showing them strictly in order, each
/// exactly once, means it does not matter whether an event arrives live or
/// through catchUp().
function accept(event) {
  if (event.seq !== app.nextSeq) return false;
  app.nextSeq += 1;
  handle(event);
  return true;
}

/// Fetch the run's full event list and show whatever we have not shown yet.
///
/// Everything here has already happened, so `replaying` keeps the window from
/// announcing it: no notification for a round that was reviewed yesterday.
async function catchUp() {
  const events = await invoke("run_events", { runId: app.runId });
  app.replaying = true;
  try {
    for (const event of events) accept(event);
  } finally {
    app.replaying = false;
  }
}

// --- Which run the window shows ---------------------------------------------

function resetView() {
  warnedAbout.clear();
  thread().replaceChildren();
  clearCards();
  app.roundOpen = false;
  app.verdict = null;
  app.lastStage = 0;
  app.cost = 0;
  app.tokens = 0;
  showRunTotal();
}

function show(id, readOnly) {
  app.runId = id;
  app.readOnly = Boolean(readOnly);
  app.nextSeq = 0;
  app.runState = null;
  remember("agent-console-run", id);
  resetView();
  $("read-only").hidden = !app.readOnly;
  return catchUp().then(() => {
    // A restored run has no live agents, so none of its cards are waiting.
    if (app.readOnly) refreshControls();
  });
}

function forgetRun() {
  app.runId = null;
  app.readOnly = false;
  $("read-only").hidden = true;
  forget("agent-console-run");
  resetView();
  setRunState(null);
  refreshWelcome();
}

// --- Wiring up the page -----------------------------------------------------

$("composer").addEventListener("submit", (submitted) => {
  submitted.preventDefault();
  sendMain();
});
$("message").addEventListener("keydown", (key) => {
  if (key.key === "Enter" && (key.metaKey || key.ctrlKey)) {
    key.preventDefault();
    sendMain();
  }
});
for (const radio of document.querySelectorAll("input[name=mode]")) {
  radio.addEventListener("change", refreshPlaceholder);
}

$("stop").addEventListener("click", () => {
  if (app.runId) invoke("stop_run", { runId: app.runId }).catch(showProblem);
});

$("new-run").addEventListener("click", () => {
  // Only a round that is actually open stands in the way. A run being read has
  // its old rounds in its history, but nothing of them is still live.
  if (app.roundOpen && !app.readOnly) {
    return showProblem("Apply or discard the current changes first.");
  }
  clearProblem();
  // The next message starts a fresh run with new agent sessions.
  forgetRun();
});

$("project").addEventListener("click", chooseFolder);
$("review-again").addEventListener("click", () => roundAction("review_again"));
$("discard").addEventListener("click", () => roundAction("discard_changes"));

wireSettings();
wireChanges();
wirePrompts();
wireHistory(async (run, opened) => {
  app.runFolder = run.project_dir;
  await setFolder(run.project_dir);
  await show(run.id, !opened.resumable);
  if (opened.resumable) {
    addToThread(el("div", "note", "Picked up where this run left off. The agents remember the conversation."));
  } else {
    addToThread(el("div", "note", "This run can be read but not continued"
      + (opened.why ? ", because " + opened.why : "") + "."));
  }
});

// --- Start-up ---------------------------------------------------------------

(async function boot() {
  const info = await invoke("app_info");
  app.settings = info.settings;
  app.defaults = info.default_settings;
  app.limits = info.limits;
  $("auth-chip").textContent = AUTH_WORDS[info.auth] || info.auth;
  buildSettingsForm(info.presets);
  buildCards(info.agents, sendToAgent);
  await setFolder(recall("agent-console-folder") || "");
  resetView();
  setRunState(null);
  refreshStages();
  refreshWelcome();

  await listen("run-event", ({ payload }) => {
    if (payload.run_id !== app.runId) return;
    // A gap means we missed something (while the page was loading, say): refetch.
    if (!accept(payload.event) && payload.event.seq > app.nextSeq) catchUp().catch(showProblem);
  });

  // If the window was reloaded mid-run, pick that run back up and replay it.
  const previous = recall("agent-console-run");
  if (previous) {
    app.runFolder = recall("agent-console-run-folder");
    show(previous).catch(forgetRun);
  }
})();
