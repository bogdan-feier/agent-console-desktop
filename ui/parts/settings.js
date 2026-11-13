// The Settings window: building the form, filling it in, and reading it back.
//
// Every value is checked again on the Rust side before it is saved, so this file
// only has to collect what the fields say.

import { $, button, el, problemText } from "./dom.js";
import { refreshModelLabels, refreshTeam } from "./cards.js";
import { app } from "./state.js";
import { invoke } from "./tauri.js";
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
  $("web-access").checked = values.web_access;
  $("use-mcp").checked = values.use_mcp_servers;
  $("resolve-conflicts").checked = values.resolve_conflicts;
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
  values.web_access = $("web-access").checked;
  values.resolve_conflicts = $("resolve-conflicts").checked;
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

/// Add one command to the allowed list and save, without opening the window.
/// Used by the "Allow this command" button on a refused step.
export async function allowCommand(command) {
  const values = { ...app.settings, allowed_commands: [...app.settings.allowed_commands, command] };
  app.settings = await invoke("save_settings", {
    settings: values,
    projectDir: app.folder || null,
    ownSettings: Boolean(app.settings.own),
  });
}

export function wireSettings() {
  $("open-settings").addEventListener("click", openSettings);
  $("settings-cancel").addEventListener("click", () => $("settings").close());
  $("settings-reset").addEventListener("click", () => fillSettingsForm(app.defaults));
  $("own-settings").addEventListener("change", refreshScope);
  $("settings-form").addEventListener("submit", (submitted) => {
    submitted.preventDefault();
    saveSettings();
  });
}
