// The project in the top left, and the welcome screen an empty conversation shows.

import { $, button, el, remember } from "./dom.js";
import { refreshModelLabels } from "./cards.js";
import { refreshControls, refreshPlaceholder } from "./controls.js";
import { app } from "./state.js";
import { invoke } from "./tauri.js";
import { thread } from "./turns.js";
import { EXAMPLES } from "./words.js";

/// Switch to a project. The Rust side gives back the settings for it, which may
/// be this project's own set rather than the general ones.
export async function setFolder(path) {
  app.folder = path || "";
  remember("agent-console-folder", app.folder);
  const name = app.folder.replace(/\/+$/, "").split("/").pop();
  $("project-name").textContent = name || "Choose a project";
  $("project-path").textContent = app.folder.replace(/^\/Users\/[^/]+/, "~");
  $("project").title = app.folder
    ? app.folder + "\nClick to choose another project"
    : "Choose the project the team works on";

  if (app.folder) {
    try {
      app.settings = await invoke("use_project", { projectDir: app.folder });
      refreshModelLabels();
      refreshControls();
    } catch (error) { /* keep the settings we have */ }
  }
  refreshWelcome();
}

export async function chooseFolder() {
  const picked = await invoke("pick_folder").catch(() => null);
  if (picked) await setFolder(picked);
}

/// What an empty conversation shows: where to start, with examples to try.
export function refreshWelcome() {
  if (app.runId !== null) return;
  const box = el("div", "welcome");
  box.append(el("h2", "", app.folder ? "What should the team work on?" : "Start by choosing a project"));
  box.append(el("p", "", app.folder
    ? "Ask a question and the dispatcher answers it. Ask for a change and the planner splits it into tasks. "
      + "Once you approve them, the workers build in separate copies of the project and the reviewer checks "
      + "their work. Nothing reaches your project until you apply it."
    : "Pick the folder of a git project in the top left. The team works on that one project."));

  if (app.folder) {
    const examples = el("div", "examples");
    for (const [exampleMode, text] of EXAMPLES) {
      examples.append(button(text, () => useExample(exampleMode, text)));
    }
    box.append(examples);
  } else {
    box.append(button("Choose a project", chooseFolder, "primary"));
  }
  thread().replaceChildren(box);
}

function useExample(exampleMode, text) {
  document.querySelector("input[name=mode][value=" + exampleMode + "]").checked = true;
  $("message").value = text;
  refreshPlaceholder();
  $("message").focus();
}
