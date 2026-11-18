// The Settings window: building the form, filling it in, and reading it back.
//
// Every value is checked again on the Rust side before it is saved, so this file
// only has to collect what the fields say.

import { $, button, el, problemText } from "./dom.js";
import { refreshModelLabels, refreshTeam } from "./cards.js";
import { app } from "./state.js";
import { canNotify, invoke, notify } from "./tauri.js";
import { EFFORT_CHOICES, MODEL_CHOICES, PRESET_WORDS } from "./words.js";

/// The roles that have a row of their own: key in the settings, label, lane colour.
const ROLES = [
  ["dispatcher", "Dispatcher", "plan-hue"],
  ["planner", "Planner", "plan-hue"],
  ["worker", "Workers", "build-hue"],
  ["reviewer", "Reviewer", "review-hue"],
];

export function buildSettingsForm(presets) {
  for (const { preset, settings: values } of presets) {
    const [label, description] = PRESET_WORDS[preset] || [preset, ""];
    const chip = button(label, () => fillModelSection(values));
    chip.title = description;
    $("presets").append(chip);
  }

  const grid = $("settings-roles");
  for (const [key, label, hue] of ROLES) {
    grid.append(el("span", "role " + hue, label));
    grid.append(modelPicker(key, label));
    grid.append(choice("effort-" + key, label + " effort", EFFORT_CHOICES));
    grid.append(turnLimit(key, label));
  }

  $("chat-turns").max = app.limits.max_turns;
  $("chat-turns").placeholder = String(app.limits.chat_turns);
  for (let number = 1; number <= app.limits.max_workers; number++) {
    const label = number === 1 ? "1 worker" : number + " workers";
    $("worker-count").append(new Option(label, String(number)));
  }
  $("notes").maxLength = app.limits.max_notes;
  $("notes").addEventListener("input", countNotes);
}

function choice(id, label, options) {
  const select = el("select");
  select.id = id;
  select.setAttribute("aria-label", label);
  for (const [value, text] of options) select.append(new Option(text, value));
  return select;
}

/// A model menu with an "Other" entry that reveals a box for any model ID.
function modelPicker(key, label) {
  const pick = el("div", "model-pick");
  const select = choice("model-" + key, label + " model", [...MODEL_CHOICES, ["other", "Other"]]);
  const custom = el("input");
  custom.type = "text";
  custom.id = "model-" + key + "-custom";
  custom.placeholder = "claude-opus-5-5";
  custom.spellcheck = false;
  custom.setAttribute("aria-label", label + " model ID");
  select.addEventListener("change", () => {
    custom.hidden = select.value !== "other";
    if (!custom.hidden) custom.focus();
  });
  pick.append(select, custom);
  return pick;
}

/// Empty means the role's default, which the placeholder shows.
function turnLimit(key, label) {
  const turns = el("input");
  turns.type = "number";
  turns.id = "turns-" + key;
  turns.min = 1;
  turns.max = app.limits.max_turns;
  turns.step = 1;
  turns.placeholder = String(app.limits[key + "_turns"]);
  turns.setAttribute("aria-label", label + " turn limit, empty for the default of " + app.limits[key + "_turns"]);
  return turns;
}

function countNotes() {
  $("notes-count").textContent = $("notes").value.length + " of " + app.limits.max_notes
    + " characters. They are sent with every turn of every agent, mostly from the cache, so shorter is cheaper.";
}

const turnsText = (value) => (value == null ? "" : String(value));

function readTurns(id) {
  const text = $(id).value.trim();
  return text === "" ? null : Number(text);
}

/// The part a preset changes: models, effort, turn limits and the spending cap.
function fillModelSection(values) {
  for (const [key] of ROLES) {
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
  $("full-shell").checked = values.full_shell;
  $("web-access").checked = values.web_access;
  showCommandList();
  $("use-mcp").checked = values.use_mcp_servers;
  $("worker-plugins").value = values.worker_plugins.join("\n");
  $("resolve-conflicts").checked = values.resolve_conflicts;
  $("skip-review").checked = values.skip_review_when_checks_pass;
  $("worker-count").value = String(values.worker_count);
  $("check-command").value = values.check_command;
  $("notes").value = values.notes;
  countNotes();
}

function readSettingsForm() {
  const values = {};
  for (const [key] of ROLES) {
    const picked = $("model-" + key).value;
    values[key] = {
      model: picked === "other" ? $("model-" + key + "-custom").value.trim() : picked,
      effort: $("effort-" + key).value || null,
      max_turns: readTurns("turns-" + key),
    };
  }
  const budget = $("budget").value.trim();
  values.max_budget_usd = budget === "" ? null : Number(budget);
  values.chat_max_turns = readTurns("chat-turns");
  values.use_mcp_servers = $("use-mcp").checked;
  // The Rust side trims each line, drops the empty ones and checks each is a folder.
  values.worker_plugins = $("worker-plugins").value.split("\n");
  values.web_access = $("web-access").checked;
  values.full_shell = $("full-shell").checked;
  values.resolve_conflicts = $("resolve-conflicts").checked;
  values.skip_review_when_checks_pass = $("skip-review").checked;
  // The Rust side trims each line and drops the empty ones and the repeats.
  values.allowed_commands = $("commands").value.split("\n");
  values.worker_count = Number($("worker-count").value);
  values.check_command = $("check-command").value;
  values.notes = $("notes").value;
  return values;
}

/// Which settings the window is editing: this project's own, or the general ones
/// every other project uses.
function refreshScope() {
  const name = app.folder.replace(/\/+$/, "").split("/").pop();
  const own = $("own-settings").checked;
  $("own-settings-label").textContent = name
    ? "Keep separate settings for " + name
    : "Keep separate settings for this project";
  $("own-settings").disabled = !app.folder;
  $("scope-note").textContent = !app.folder
    ? "No project chosen, so this is the general set."
    : own
      ? "Saving changes this project only. Other projects keep the general settings."
      : "Saving changes the general settings, which every project without its own set uses.";
}

export function openSettings() {
  $("notification-note").textContent = "";
  $("suggest-note").textContent = "";
  fillSettingsForm(app.settings);
  $("own-settings").checked = Boolean(app.settings.own);
  refreshScope();
  $("settings").showModal();
}

async function saveSettings() {
  try {
    app.settings = await invoke("save_settings", {
      settings: readSettingsForm(),
      projectDir: app.folder || null,
      ownSettings: $("own-settings").checked,
    });
    $("settings").close();
    refreshModelLabels();
    refreshTeam();
  } catch (error) {
    $("settings-notice").textContent = problemText(error);
  }
}

/// Offer the commands this kind of project usually needs, and its check command.
/// They go into the boxes, not into the settings: saving is still a decision.
async function suggestCommands() {
  const note = $("suggest-note");
  if (!app.folder) {
    note.textContent = "Choose a project first.";
    return;
  }
  const found = await invoke("project_suggestions", { projectDir: app.folder });
  if (!found.kinds.length) {
    note.textContent = "Nothing recognised in this project, so there is nothing to suggest. "
      + "Add the commands you use by hand.";
    return;
  }
  const already = $("commands").value.split("\n").map((line) => line.trim());
  const added = found.commands.filter((command) => !already.includes(command));
  if (added.length) {
    $("commands").value = already.concat(added).filter(Boolean).join("\n");
  }
  let said = added.length
    ? found.kinds.join(" and ") + " project: added " + added.length + " command" + (added.length === 1 ? "" : "s")
    : found.kinds.join(" and ") + " project: its usual commands are already here";
  if (found.check && !$("check-command").value.trim()) {
    $("check-command").value = found.check;
    said += ", and filled in the check command";
  }
  note.textContent = said + ". Look them over, then save.";
}

/// Ask macOS, send one, and say plainly what happened. This is also how the
/// permission box is brought up on purpose, rather than waiting for a round.
async function testNotification() {
  const note = $("notification-note");
  note.textContent = "Asking macOS...";
  const allowed = await canNotify();
  if (!allowed) {
    note.textContent = "macOS is not allowing notifications from Agent Console. Turn them on in "
      + "System Settings, under Notifications, then try again.";
    return;
  }
  await notify("Agent Console", "That is what a finished round will look like.");
  note.textContent = "Sent one just now. If nothing appeared, check Agent Console under "
    + "Notifications in System Settings, and that Do Not Disturb is off.";
}

/// With "any command" on, the list below it decides nothing, so grey it out rather
/// than leave the user wondering which of the two is in force.
function showCommandList() {
  const unrestricted = $("full-shell").checked;
  $("commands").disabled = unrestricted;
  $("suggest-commands").disabled = unrestricted;
}

export function wireSettings() {
  $("full-shell").addEventListener("change", showCommandList);
  $("open-settings").addEventListener("click", openSettings);
  $("test-notification").addEventListener("click", testNotification);
  $("suggest-commands").addEventListener("click", () => {
    suggestCommands().catch((error) => { $("suggest-note").textContent = problemText(error); });
  });
  $("settings-cancel").addEventListener("click", () => $("settings").close());
  $("settings-reset").addEventListener("click", () => fillSettingsForm(app.defaults));
  $("own-settings").addEventListener("change", refreshScope);
  $("settings-form").addEventListener("submit", (submitted) => {
    submitted.preventDefault();
    saveSettings();
  });
}
