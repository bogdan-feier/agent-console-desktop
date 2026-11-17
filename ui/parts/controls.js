// What the window shows about the run as a whole: the step tracker in the top
// bar, which buttons are usable, and the round bar above the message box.

import { $ } from "./dom.js";
import { clearCards, refreshTeam, showTaskTitles } from "./cards.js";
import { agents, app, isAtRest, isBusy } from "./state.js";
import { thread } from "./turns.js";
import { notify } from "./tauri.js";
import { PLACEHOLDER, STATE_WORDS } from "./words.js";

/// What the message box sends: dispatcher, brainstorm or plan.
export const mode = () => document.querySelector("input[name=mode]:checked").value;

/// A plan that has been approved (or replaced) is history: no editing, no buttons.
export function lockPlans() {
  for (const plan of thread().querySelectorAll(".decision.plan")) {
    for (const node of plan.querySelectorAll("button, .hint")) node.remove();
    for (const field of plan.querySelectorAll("input, textarea")) field.readOnly = true;
  }
}

/// The titles of the newest plan's tasks, in order.
export function setRunState(state) {
  const previous = app.runState;
  app.runState = state;
  $("run-state").textContent = state ? STATE_WORDS[state] : "No run yet";
  if (state === "building") {
    lockPlans();
    // The titles come from the round itself, which carries them whether they were
    // planned, edited before approving, or typed as a quick change.
    showTaskTitles(app.taskTitles);
  }
  if (state !== previous) tellTheUser(state);
  refreshStages();
  refreshControls();
}

/// A round takes minutes, so the user is probably in another app. Say something
/// only when the run stops needing the machine and starts needing them.
const CALLS_FOR_YOU = {
  awaiting_approval: ["Plan ready", "The planner wrote a plan. Approve it to start the workers."],
  done: ["Review done", "The round's changes are waiting: apply them, discard them, or send work back."],
  failed: ["Something went wrong", "The run stopped on an error."],
};

function tellTheUser(state) {
  const message = CALLS_FOR_YOU[state];
  if (!message || app.replaying) return; // replaying an old run is not news
  // hasFocus(), not hidden: a window can be in plain sight behind another app's,
  // and someone looking at that other app still wants to be told.
  if (!document.hasFocus()) notify(message[0], message[1]);
}

export function setRound(open, quick = false, tasks = []) {
  app.quickRound = open && quick;
  // The round says what each worker is doing. A quick change has no plan card to
  // read it back out of, so this is the only place its title comes from.
  app.taskTitles = open ? tasks : [];
  if (open && tasks.length) showTaskTitles(tasks);
  app.roundOpen = open;
  if (open) {
    // A new round starts from clean cards.
    clearCards();
    app.verdict = null;
  } else {
    // The workers' and reviewer's sessions ended with the round.
    for (const agent of Object.values(agents)) if (agent.card) agent.hasWorked = false;
    app.roundClosed = true;
    for (const old of document.querySelectorAll(".decision.review button")) old.remove();
    // Nothing is waiting on that round any more, so neither is any plan.
    for (const hint of thread().querySelectorAll(".round-wait")) hint.remove();
  }
  refreshTeam(); // the idle panels read differently in a quick round
  refreshStages();
  refreshControls();
}

/// Light up the step of the round we are at.
export function refreshStages() {
  let stage = 0;
  if (app.runState === "planning") stage = 1;
  else if (app.runState === "awaiting_approval") stage = 2;
  else if (app.runState === "building") stage = 3;
  else if (app.runState === "reviewing") stage = 4;
  else if (app.roundOpen) stage = app.runState === "chatting" ? app.lastStage : 5;

  const halted = app.runState === "stopped" || app.runState === "failed";
  if (halted) stage = app.lastStage; else app.lastStage = stage;

  $("stages").querySelectorAll("li").forEach((item, index) => {
    item.classList.toggle("passed", app.runId !== null && index < stage);
    item.classList.toggle("current", app.runId !== null && index === stage);
    item.classList.toggle("halted", halted && index === stage);
    if (index === stage) item.setAttribute("aria-current", "step");
    else item.removeAttribute("aria-current");
  });
}

/// Enable exactly what can be done right now, and nothing else.
export function refreshControls() {
  const busy = isBusy();
  const rest = isAtRest();
  $("send").hidden = busy;
  $("stop").hidden = !busy;
  $("send").disabled = app.readOnly;
  $("message").disabled = app.readOnly;
  $("new-run").disabled = busy || app.runId === null;
  $("project").disabled = busy;

  for (const agent of Object.values(agents)) {
    if (!agent.card || !agent.card.form) continue;
    // Workers can be messaged during the round they worked in.
    const usable = rest && agent.hasWorked && app.roundOpen;
    for (const control of agent.card.form.querySelectorAll("textarea, button")) control.disabled = !usable;
  }

  // Showing the round bar makes the conversation shorter; keep its end in view.
  const box = thread();
  const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  $("round-bar").hidden = !(app.roundOpen && rest);
  if (atEnd) box.scrollTop = box.scrollHeight;

  $("round-text").textContent =
    app.verdict === "approve" ? "The reviewer approved these changes."
      : app.verdict === "needs_changes" ? "The reviewer asked for changes."
        : "The workers' changes are ready.";

  // Buttons in the conversation and on the reviewer's card say what they need in a
  // data-needs attribute.
  for (const node of document.querySelectorAll("[data-needs]")) {
    const needsApproval = node.dataset.needs === "approval";
    node.disabled = needsApproval
      ? app.runState !== "awaiting_approval" || app.roundOpen || app.readOnly
      : !(rest && app.roundOpen);
  }
  refreshTeam();
  refreshPlaceholder();
}

export function refreshPlaceholder() {
  const chosen = mode();
  $("message").placeholder =
    chosen === "plan" && app.runId === null ? PLACEHOLDER.planFirst : PLACEHOLDER[chosen];
}
