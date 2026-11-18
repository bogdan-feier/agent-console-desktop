// The cards that wait for you: the plan to approve and the result of the project's
// own checks in the conversation, and the reviewer's verdict on the reviewer's card.

import { button, clearProblem, el, showProblem } from "./dom.js";
import { lockPlans, refreshControls } from "./controls.js";
import { render, rich } from "./markdown.js";
import { agents, app } from "./state.js";
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
  if (!app.replaying) offerCheckCommand(approve).catch(() => { /* only an offer */ });
}

/// Without a check command, a slip the tests would catch costs a review and a
/// second worker turn to find. When the project has an obvious one, offer it here,
/// where the round is about to start, rather than only in Settings.
async function offerCheckCommand(approve) {
  if (app.settings.check_command.trim() || !app.folder) return;
  const found = await invoke("project_suggestions", { projectDir: app.folder });
  if (!found.check) return;

  const line = el("p", "hint check-offer");
  line.append("No check command is set, so nothing runs the tests before the reviewer looks. Run ",
    el("code", "", found.check), " on every round? ");
  const use = button("Use it", async () => {
    try {
      app.settings = await invoke("save_settings", {
        settings: { ...app.settings, check_command: found.check },
        projectDir: app.folder,
        ownSettings: Boolean(app.settings.own),
      });
      line.replaceChildren("Every round now runs ", el("code", "", found.check),
        ". You can change it in Settings.");
    } catch (error) {
      showProblem(error);
    }
  });
  line.append(use);
  approve.before(line);
}

/// More files than this and the list starts folded.
const FILES_SHOWN_OPEN = 8;

/// One editable task: title, instructions, and the files its worker may touch.
function taskFields(task, number) {
  const card = el("div", "task");
  card.append(el("div", "goes-to", "Task " + number + ", for Worker " + number));

  const title = el("input", "task-name");
  title.type = "text";
  title.value = task.title;
  title.dataset.field = "title";
  title.setAttribute("aria-label", "Task " + number + " title");

  const instructions = el("textarea");
  instructions.value = task.instructions;
  instructions.dataset.field = "instructions";
  instructions.setAttribute("aria-label", "Task " + number + " instructions");

  // One file per line, so the whole list is visible rather than scrolled sideways.
  const files = el("textarea", "files");
  files.value = task.files.join("\n");
  files.dataset.field = "files";
  files.setAttribute("aria-label", "Files for task " + number + ", one per line");

  // The planner's call that this task needs judgment. It runs on Sonnet when the
  // workers are on Haiku; any other worker model is left as Settings has it.
  const hardSwitch = el("label", "switch");
  const hard = el("input");
  hard.type = "checkbox";
  hard.checked = Boolean(task.hard);
  hard.dataset.field = "hard";
  hardSwitch.append(hard, el("span", null, "Needs judgment "),
    el("span", "hint", "(runs on Sonnet instead of Haiku)"));

  // A long list is folded, so the instructions above it stay the first thing you read.
  const fileList = el("details", "file-list");
  fileList.open = task.files.length <= FILES_SHOWN_OPEN;
  fileList.append(el("summary", "field-label", "Files it may change (" + task.files.length + ")"), files);
  fileList.addEventListener("toggle", () => fit(files));

  card.append(title, el("div", "field-label", "Instructions"), readable(instructions), instructions,
    fileList, hardSwitch);
  growToFit(instructions);
  growToFit(files);

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
    // A closed <details> has no height to measure, so size it when opened.
    found.addEventListener("toggle", () => fit(text));
    growToFit(text);
  }
  return card;
}

/// The instructions shown as formatted Markdown; clicking them swaps in the
/// textarea to edit, and leaving the textarea shows the formatted version again.
function readable(area) {
  const view = el("div", "say task-text");
  view.title = "Click to edit";
  view.tabIndex = 0;
  const show = () => {
    view.innerHTML = render(area.value);
    view.hidden = false;
    area.hidden = true;
  };
  const edit = () => {
    view.hidden = true;
    area.hidden = false;
    fit(area);
    area.focus();
  };
  view.addEventListener("click", edit);
  view.addEventListener("keydown", (key) => {
    if (key.key === "Enter") {
      key.preventDefault();
      edit();
    }
  });
  area.addEventListener("blur", show);
  show();
  return view;
}

/// A textarea as tall as its text, so a task reads in full without scrolling.
function fit(area) {
  area.style.height = "auto";
  area.style.height = area.scrollHeight + 2 + "px";
}

function growToFit(area) {
  area.addEventListener("input", () => fit(area));
  // Not measurable until the card is on the page.
  requestAnimationFrame(() => fit(area));
}

async function approvePlan(box) {
  const tasks = [...box.querySelectorAll(".task")].map((card) => ({
    title: card.querySelector("[data-field=title]").value,
    instructions: card.querySelector("[data-field=instructions]").value,
    files: card.querySelector("[data-field=files]").value.split(/[\n,]/).map((file) => file.trim()).filter(Boolean),
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

/// Add one command to the allowed list and save, without opening the window.
/// Used by the Allow buttons on a refused step and on the review.
export async function allowCommand(command) {
  const values = { ...app.settings, allowed_commands: [...app.settings.allowed_commands, command] };
  app.settings = await invoke("save_settings", {
    settings: values,
    projectDir: app.folder || null,
    ownSettings: Boolean(app.settings.own),
  });
}

/// Whether allowing a command could make any difference. With "any command" on, the
/// allowed list is not used: what is still refused is on the app's own never-allow
/// list, which no setting lifts.
export const allowingHelps = () => !app.settings.full_shell;

/// What the worker of `task` was refused in its latest session. A finding that
/// needs one of these comes back unfixed however often it is sent back, so the
/// review says so and offers to allow them first.
function refusals(task) {
  const refused = [...(agents["worker-" + task]?.refused ?? [])];
  if (!refused.length) return null;
  const line = el("p", "hint refusals");
  if (!allowingHelps()) {
    line.append("Worker " + task + " was refused ",
      ...refused.flatMap((rule, index) => [index ? ", " : "", el("code", "", rule)]),
      ", which the app never allows. Sending back will not fix a finding that needs it: do that part yourself.");
    return line;
  }
  line.append("Worker " + task + " was refused these commands. If a finding needs one, allow it before sending back: ");
  for (const rule of refused) {
    const allow = button("Allow \"" + rule + "\"", async () => {
      allow.disabled = true;
      try {
        await allowCommand(rule);
        agents["worker-" + task].refused.delete(rule);
        allow.replaceWith(el("span", "allowed", "Allowed " + rule));
      } catch (error) {
        allow.replaceWith(el("span", "problem", String(error)));
      }
    }, "allow");
    line.append(allow, " ");
  }
  return line;
}

/// One group per task, each with a button that sends it back to that task's worker.
function findings(mustFix) {
  const box = el("div", "findings");
  for (const task of [...new Set(mustFix.map((finding) => finding.task))].sort()) {
    const group = el("div", "finding-group");
    // Task 0 is the reviewer saying the file belongs to no task. The app gives those
    // to the first worker that finished, which is then allowed to edit them.
    const unassigned = task === 0;
    group.append(el("div", "goes-to", unassigned
      ? "No task covered these files"
      : "Must fix in task " + task));
    const list = el("ul");
    for (const finding of mustFix.filter((one) => one.task === task)) list.append(rich("li", finding.issue));
    group.append(list);
    if (!unassigned) {
      const refused = refusals(task);
      if (refused) group.append(refused);
    }

    const sendBack = button(unassigned ? "Give these to a worker" : "Send back to Worker " + task, () => {
      clearProblem();
      invoke("send_back", { runId: app.runId, worker: task }).catch(showProblem);
    });
    if (unassigned) sendBack.title = "The first worker that finished fixes them, then the reviewer checks again";
    sendBack.dataset.needs = "round";
    group.append(sendBack);
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
