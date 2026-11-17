// The cards that wait for you: the plan to approve and the result of the project's
// own checks in the conversation, and the reviewer's verdict on the reviewer's card.

import { button, clearProblem, el, showProblem } from "./dom.js";
import { lockPlans, refreshControls } from "./controls.js";
import { rich } from "./markdown.js";
import { app } from "./state.js";
import { invoke } from "./tauri.js";
import { addToThread, addToTurn, thread } from "./turns.js";
import { plural } from "./words.js";

/// Decisions wait for you, so scroll the conversation to the top of a new one.
function reveal(box) {
  thread().scrollTop = box.offsetTop - 12;
}

export function showPlan(event) {
  // A newer plan replaces any earlier one that was never approved.
  for (const older of thread().querySelectorAll(".decision.plan")) older.classList.add("superseded");
  lockPlans();

  const box = el("div", "decision plan plan-hue");
  box.append(el("h3", "", "Plan: " + plural(event.tasks.length, "task")));
  box.append(rich("p", event.summary));
  event.tasks.forEach((task, index) => box.append(taskFields(task, index + 1)));

  const approve = button("Approve and start the workers", () => approvePlan(box), "primary");
  approve.dataset.needs = "approval";
  approve.style.justifySelf = "start";
  box.append(approve);
  box.append(el("p", "hint",
    "You can edit the tasks above before approving. For a different plan, choose Plan below, "
    + "say what to change, and send."));
  // Approving opens a round, and only one round is open at a time, so say why
  // the button is greyed out rather than leaving the user to guess.
  if (app.roundOpen) {
    box.append(el("p", "hint round-wait",
      "Approving waits on the round below: apply or discard those changes first."));
  }

  addToThread(box);
  refreshControls();
  reveal(box);
}

/// One editable task: title, instructions, and the files its worker may touch.
function taskFields(task, number) {
  const card = el("div", "task");
  card.append(el("div", "goes-to", "Task " + number + ", for Worker " + number));

  const title = el("input");
  title.type = "text";
  title.value = task.title;
  title.dataset.field = "title";
  title.setAttribute("aria-label", "Task " + number + " title");

  const instructions = el("textarea");
  instructions.value = task.instructions;
  instructions.dataset.field = "instructions";
  instructions.setAttribute("aria-label", "Task " + number + " instructions");

  const files = el("input", "files");
  files.type = "text";
  files.value = task.files.join(", ");
  files.dataset.field = "files";
  files.setAttribute("aria-label", "Files for task " + number + ", separated by commas");

  // The planner's call that this task needs judgment. It runs on Sonnet when the
  // workers are on Haiku; any other worker model is left as Settings has it.
  const hardSwitch = el("label", "switch");
  const hard = el("input");
  hard.type = "checkbox";
  hard.checked = Boolean(task.hard);
  hard.dataset.field = "hard";
  hardSwitch.append(hard, el("span", null, "Needs judgment "),
    el("span", "hint", "(runs on Sonnet instead of Haiku)"));

  card.append(title, instructions, files, hardSwitch);

  // What the planner read, passed on so the worker does not read it again. Folded
  // away: it is usually right and rarely interesting, but when a worker goes wrong
  // this is the first place to look, and it is editable like everything else here.
  if (task.context && task.context.trim()) {
    const found = el("details", "found");
    found.append(el("summary", null, "What the planner found (sent to the worker)"));
    const text = el("textarea");
    text.value = task.context;
    text.dataset.field = "context";
    text.setAttribute("aria-label", "What the planner found for task " + number);
    found.append(text);
    card.append(found);
  }
  return card;
}

async function approvePlan(box) {
  const tasks = [...box.querySelectorAll(".task")].map((card) => ({
    title: card.querySelector("[data-field=title]").value,
    instructions: card.querySelector("[data-field=instructions]").value,
    files: card.querySelector("[data-field=files]").value.split(",").map((file) => file.trim()).filter(Boolean),
    context: card.querySelector("[data-field=context]")?.value ?? "",
    hard: card.querySelector("[data-field=hard]").checked,
  }));
  clearProblem();
  try {
    await invoke("approve_plan", { runId: app.runId, tasks });
  } catch (error) {
    showProblem(error);
  }
}

export function showReview(agent, event) {
  for (const old of document.querySelectorAll(".decision.review button")) old.remove();
  app.verdict = event.verdict;

  const box = el("div", "decision review review-hue verdict-" + event.verdict);
  box.append(el("h3", "", event.verdict === "approve" ? "Review: approved" : "Review: needs changes"));
  box.append(rich("p", event.summary));
  if (event.must_fix && event.must_fix.length) box.append(findings(event.must_fix));
  if (event.consider && event.consider.length) {
    box.append(el("div", "goes-to", "Worth considering"));
    const list = el("ul");
    for (const item of event.consider) list.append(rich("li", item));
    box.append(list);
  }
  // On the reviewer's card, under the work that led to it.
  addToTurn(agent, box);
  refreshControls();
  box.scrollIntoView({ block: "nearest" });
}

/// One group per task, each with a button that sends it back to that task's worker.
function findings(mustFix) {
  const box = el("div", "findings");
  for (const task of [...new Set(mustFix.map((finding) => finding.task))].sort()) {
    const group = el("div", "finding-group");
    // Task 0 is the reviewer saying the file belongs to no task, so no worker is
    // allowed to change it. Offering "send back" there would only get it refused.
    const mine = task === 0;
    group.append(el("div", "goes-to", mine
      ? "Yours to fix: no task covered these files"
      : "Must fix in task " + task));
    const list = el("ul");
    for (const finding of mustFix.filter((one) => one.task === task)) list.append(rich("li", finding.issue));
    group.append(list);

    if (!mine) {
      const sendBack = button("Send back to Worker " + task, () => {
        clearProblem();
        invoke("send_back", { runId: app.runId, worker: task }).catch(showProblem);
      });
      sendBack.dataset.needs = "round";
      group.append(sendBack);
    }
    box.append(group);
  }
  return box;
}

/// The project's own test or build command, run by the app rather than an agent.
export function showChecks(event) {
  const box = el("div", "decision checks " + (event.passed ? "passed" : "failed"));
  box.append(el("h3", "", event.passed ? "Checks passed" : "Checks failed"));
  const line = el("p");
  line.append(el("code", "", event.command), " " + event.note);
  box.append(line);
  if (event.output.trim()) {
    const fold = el("details");
    fold.open = !event.passed;
    fold.append(el("summary", "", "Output"), el("pre", "", event.output));
    box.append(fold);
  }
  addToThread(box);
}
