// The Runs window: every run this app has saved, what it cost, and a way back
// into one. Runs from an earlier launch open read-only — their agents' sessions
// and worktrees are gone, so they can be read but not continued.

import { $, el, showProblem } from "./dom.js";
import { app } from "./state.js";
import { invoke } from "./tauri.js";
import { count, plural, PRESET_WORDS, STATE_WORDS } from "./words.js";

/// Set at start-up: how the page shows a run it has just loaded.
let onOpen = () => {};

function when(seconds) {
  const date = new Date(seconds * 1000);
  const today = new Date();
  const sameDay = date.toDateString() === today.toDateString();
  return sameDay
    ? date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : date.toLocaleDateString([], { month: "short", day: "numeric" });
}

export async function openHistory() {
  const list = $("run-list");
  list.replaceChildren(el("p", "hint", "Reading saved runs..."));
  $("history-notice").textContent = "";
  $("history").showModal();

  let runs = [];
  try {
    runs = await invoke("recent_runs");
  } catch (error) {
    $("history-notice").textContent = typeof error === "string" ? error : String(error);
    return;
  }

  list.replaceChildren();
  if (!runs.length) {
    list.append(el("p", "hint", "No runs saved yet. They are kept from now on, one file per run."));
    return;
  }

  const total = runs.reduce((sum, run) => sum + run.tokens, 0);
  const spent = runs.reduce((sum, run) => sum + run.cost_usd, 0);
  $("history-total").textContent =
    plural(runs.length, "run") + ", " + count(total) + " tokens in all"
    + (spent > 0 ? ", $" + spent.toFixed(2) : "");

  // The widest run sets the scale of the bars, so they compare at a glance.
  const widest = Math.max(...runs.map((run) => run.tokens), 1);
  for (const run of runs) list.append(runRow(run, widest));
}

function runRow(run, widest) {
  const row = el("button", "run-row");
  row.type = "button";
  if (run.id === app.runId) row.classList.add("chosen");

  const name = run.project_dir.replace(/\/+$/, "").split("/").pop();
  const head = el("div", "run-head");
  head.append(el("span", "when", when(run.started_at)), el("span", "where", name));
  if (run.rounds) head.append(el("span", "rounds", plural(run.rounds, "round")));
  const how = setup(run.models);
  if (how) {
    const chip = el("span", "run-setup", how.short);
    chip.title = how.full;
    head.append(chip);
  }
  row.append(head);
  row.append(el("div", "run-request", run.request));

  const bar = el("div", "run-bar");
  const fill = el("div", "run-bar-fill");
  fill.style.width = Math.max(2, Math.round((run.tokens / widest) * 100)) + "%";
  bar.append(fill);
  const numbers = el("div", "run-numbers");
  numbers.append(
    el("span", "", count(run.tokens) + " tokens" + (run.cost_usd > 0 ? ", $" + run.cost_usd.toFixed(2) : "")),
    el("span", "run-state-word", STATE_WORDS[run.state] || run.state),
  );
  row.append(bar, numbers);

  row.title = run.project_dir;
  row.addEventListener("click", async () => {
    try {
      const opened = await invoke("open_run", { runId: run.id });
      $("history").close();
      onOpen(run, opened);
    } catch (error) {
      $("history-notice").textContent = typeof error === "string" ? error : String(error);
    }
  });
  return row;
}

/// How a run was set up, for comparing one run's usage with another's. Runs saved
/// before the app kept this have none.
function setup(models) {
  if (!models) return null;
  const role = (chosen) => chosen.model + (chosen.effort ? " at " + chosen.effort + " effort" : "");
  const full = "Planner " + role(models.planner) + ", workers " + role(models.worker)
    + ", reviewer " + role(models.reviewer) + ", up to " + plural(models.workers, "worker");
  const preset = models.preset && (PRESET_WORDS[models.preset] || [])[0];
  return { short: preset || models.planner.model + " and " + models.worker.model, full };
}

export function wireHistory(open) {
  onOpen = open;
  $("open-history").addEventListener("click", () => openHistory().catch(showProblem));
  $("history-close").addEventListener("click", () => $("history").close());
}
