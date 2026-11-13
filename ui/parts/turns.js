// How an agent's work is drawn, wherever it appears.
//
// A "turn" is one run of one agent: a heading saying who is working, the tool
// steps it took (folded away, since they are usually noise), and what it wrote.
// The dispatcher's and planner's turns go in the conversation; everyone else's
// go on their card. Both are scrolling boxes, so the code is the same.

import { $, append, el } from "./dom.js";
import { rich } from "./markdown.js";
import { agents, app, isChat, modelLabel } from "./state.js";
import { count, HUE, STATUS_WORDS } from "./words.js";

export const thread = () => $("thread");

export function addToThread(node) {
  const welcome = thread().querySelector(".welcome");
  if (welcome) welcome.remove();
  append(thread(), node);
}

export function addToCard(agent, node) {
  const empty = agent.card.log.querySelector(".empty");
  if (empty) empty.remove();
  agent.card.root.hidden = false;
  append(agent.card.log, node);
}

/// The box an agent's output lands in.
const boxOf = (agent) => (isChat(agent) ? thread() : agent.card.log);

export function youSaid(agent, text) {
  if (agent.card) return addToCard(agent, el("div", "you", text));
  // When the dispatcher hands a message on, the planner receives the same words:
  // show them once. (Only agent turns may stand between the two.)
  const nodes = [...thread().children];
  const last = nodes.filter((node) => node.matches(".you")).pop();
  const onlyTurnsSince = last && nodes.slice(nodes.indexOf(last) + 1).every((node) => node.matches(".turn"));
  if (last && last.textContent === text && onlyTurnsSince) return;
  addToThread(el("div", "you", text));
}

export function startTurn(agent, running) {
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

/// Where something an agent says goes. Some of it arrives just after its session
/// ended (the dispatcher's reply, a review): that belongs to the turn that just
/// finished, as long as nothing else has been shown since.
export function turnOf(agent) {
  if (agent.turn) return agent.turn;
  const last = agent.lastTurn;
  if (last && last.root.parentElement && !last.root.nextElementSibling) return last;
  return startTurn(agent, false);
}

export function addToTurn(agent, node) {
  const turn = turnOf(agent);
  turn.root.append(node);
  const box = boxOf(agent);
  if (box.scrollHeight - box.scrollTop - box.clientHeight < 160) box.scrollTop = box.scrollHeight;
}

export function addStep(agent, node, text) {
  const turn = turnOf(agent);
  turn.count += 1;
  turn.last = text;
  turn.list.append(node);
  turn.steps.hidden = false;
  refreshSteps(turn, turn.running);
}

/// While it works, the fold shows the step it is on; afterwards, how many there were.
function refreshSteps(turn, running) {
  const word = turn.count === 1 ? " step" : " steps";
  turn.summary.textContent = running ? turn.last : turn.count + word;
  turn.summary.title = running ? turn.count + word + " so far" : "";
}

export function endTurn(agent, status) {
  const turn = agent.turn;
  if (!turn) return;
  turn.state.className = "";
  turn.state.textContent = status === "done" ? "" : STATUS_WORDS[status] || status;
  turn.running = false;
  refreshSteps(turn, false);
  agent.turn = null;
  agent.lastTurn = turn;
}

/// Where a notice or an error goes. What the app says for the reviewer while it
/// is not running is about the whole round (merging, applying, discarding), so
/// it goes in the conversation, where you are deciding.
export function place(agent, node) {
  if (agent.turn) addToTurn(agent, node);
  else if (isChat(agent) || agent.role === "reviewer") addToThread(node);
  else addToCard(agent, node);
}

/// Anything with no agent of its own belongs to the run: show it in the conversation.
export function placeRunWide(node) {
  addToThread(node);
}

// --- Usage ------------------------------------------------------------------

/// One small line per finished session, and a running total for the run in the bar.
export function showUsage(agent, event) {
  const used = event.tokens;
  const total =
    used.input_tokens + used.output_tokens + used.cache_read_input_tokens + used.cache_creation_input_tokens;
  let line = count(total) + " tokens, " + count(used.cache_read_input_tokens) + " from cache";
  if (event.turns != null) line += ", " + event.turns + (event.turns === 1 ? " turn" : " turns");
  if (event.cost_usd != null) line += ", $" + event.cost_usd.toFixed(2);

  const node = el("div", "tokens", line);
  node.title =
    "New input " + count(used.input_tokens + used.cache_creation_input_tokens) +
    ", output " + count(used.output_tokens) +
    ". Cache reads are the cheap kind: the part of the prompt that was the same as the turn before. " +
    "Dollars are API prices.";
  addToTurn(agent, node);

  app.tokens += total;
  app.cost += event.cost_usd || 0;
  showRunTotal();
}

export function showRunTotal() {
  const usage = $("run-usage");
  usage.hidden = app.tokens === 0;
  usage.textContent = "This run: " + count(app.tokens) + " tokens" + (app.cost > 0 ? ", $" + app.cost.toFixed(2) : "");
  usage.title =
    "API-equivalent cost. On a Claude plan login this is a measure of how much of your limits the run used.";
}

/// Forget every turn in progress (a new round, or a different run).
export function clearTurns() {
  for (const agent of Object.values(agents)) {
    agent.turn = null;
    agent.lastTurn = null;
  }
}

export { rich };
