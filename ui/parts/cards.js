// The team's cards on the right: one per worker, one for the reviewer, and one
// for the resolver that only appears when it is needed.

import { $, el } from "./dom.js";
import { agents, app, isChat, modelLabel } from "./state.js";
import { clearTurns } from "./turns.js";
import { CARD_EMPTY, HUE, plural, STATUS_WORDS } from "./words.js";

/// Set by the page at start-up: what to do when you send a card's message box.
let onMessage = () => {};

export function buildCards(list, send) {
  onMessage = send;
  for (const info of list) {
    const agent = { ...info, number: Number(info.id.split("-")[1]) || 0, turn: null, hasWorked: false };
    agents[info.id] = agent;
    if (isChat(agent)) continue;
    agent.card = buildCard(agent);
  }
}

function buildCard(agent) {
  const root = el("section", "card " + HUE[agent.role]);
  const header = el("header");
  const titleRow = el("div", "title-row");
  const status = el("span", "status");
  status.append(el("span", "dot"), el("span", "status-word"));
  titleRow.append(el("h2", "", agent.label), status);
  const task = el("div", "task-title");
  header.append(titleRow, el("div", "model"), task);
  const log = el("div", "log");
  root.append(header, log);

  // Only workers get a message box. The resolver runs inside a merge that is
  // half-done, and the reviewer's verdict is final: to have it look again, use
  // "Review again" in the round bar.
  let form = null;
  let input = null;
  if (agent.role === "worker") {
    const footer = el("footer");
    form = el("form", "message-form");
    input = el("textarea");
    input.rows = 1;
    input.placeholder = "Message " + agent.label;
    input.setAttribute("aria-label", "Message " + agent.label);
    const send = el("button", "", "Send");
    send.type = "submit";
    form.append(input, send);
    form.addEventListener("submit", (submitted) => {
      submitted.preventDefault();
      onMessage(agent, input);
    });
    input.addEventListener("keydown", (key) => {
      if (key.key === "Enter" && (key.metaKey || key.ctrlKey)) {
        key.preventDefault();
        form.requestSubmit();
      }
    });
    footer.append(form);
    root.append(footer);
  }

  const slot = agent.role === "worker" ? $("workers") : $("reviewer-slot");
  slot.append(root);
  if (agent.role === "resolver") root.hidden = true;
  return {
    root,
    log,
    task,
    model: header.querySelector(".model"),
    word: status.querySelector(".status-word"),
    form,
    input,
  };
}

export function clearCards() {
  clearTurns();
  for (const agent of Object.values(agents)) {
    if (!agent.card) continue;
    agent.card.log.replaceChildren(el("p", "empty", CARD_EMPTY[agent.role]));
    agent.card.task.textContent = "";
    agent.hasWorked = false;
    if (agent.role === "resolver") agent.card.root.hidden = true;
    setStatus(agent, "idle");
  }
  refreshTeam();
}

export function setStatus(agent, status) {
  // "error" can mean it stopped at a turn limit or at the spending cap; its
  // session is kept, so it can be told to continue.
  if (status === "done" || status === "error") agent.hasWorked = true;
  if (!agent.card) return;
  agent.card.root.dataset.status = status;
  agent.card.word.textContent = STATUS_WORDS[status] || status;
  if (status === "idle") agent.card.model.textContent = modelLabel(agent.role);
}

/// Workers take their task title from the plan that was approved.
export function showTaskTitles(titles) {
  for (const agent of Object.values(agents)) {
    if (agent.role !== "worker") continue;
    const title = titles[agent.number - 1];
    agent.card.task.textContent = title ? "Task " + agent.number + ": " + title : "No task this round";
    agent.card.task.title = title || "";
  }
  refreshTeam();
}

/// Workers above the "most workers per plan" setting sit out; dim them while idle.
export function refreshTeam() {
  for (const agent of Object.values(agents)) {
    if (agent.role !== "worker") continue;
    const off = agent.number > app.settings.worker_count && agent.card.root.dataset.status === "idle";
    agent.card.root.classList.toggle("off", off);
    const empty = agent.card.log.querySelector(".empty");
    if (!empty) continue;
    if (off) {
      empty.textContent = "Not used: Settings allows " + plural(app.settings.worker_count, "worker") + " per plan.";
    } else if (agent.card.task.textContent === "No task this round") {
      // A quick change has no plan at all, so do not blame one.
      empty.textContent = app.quickRound
        ? "A quick change uses one worker, so this one sits out."
        : "The plan has fewer tasks than there are workers, so this one sits out.";
    } else {
      empty.textContent = CARD_EMPTY.worker;
    }
  }

  // A quick change never calls the reviewer, so its panel should not say it is
  // about to look at anything.
  const reviewer = agents.reviewer;
  if (reviewer && reviewer.card && reviewer.card.root.dataset.status === "idle") {
    const empty = reviewer.card.log.querySelector(".empty");
    if (empty) {
      empty.textContent = app.quickRound
        ? "Not used: a quick change is not reviewed. Use \"Review again\" below if you want it looked at."
        : CARD_EMPTY.reviewer;
    }
  }
}

export function refreshModelLabels() {
  for (const agent of Object.values(agents)) {
    if (agent.card && agent.card.root.dataset.status === "idle") {
      agent.card.model.textContent = modelLabel(agent.role);
    }
  }
}
