// Prompts you keep: the requests you type often, as chips above the message box.
// They live in this browser profile, not in the project.

import { $, button, el, recallList, rememberList } from "./dom.js";

const KEY = "agent-console-prompts";
const MAX = 12;
const LABEL = 48;

const load = () => recallList(KEY);

function save(list) {
  rememberList(KEY, list.slice(0, MAX));
  refreshPrompts();
}

export function refreshPrompts() {
  const row = $("prompts");
  const list = load();
  const typed = $("message").value.trim();
  const canSave = typed.length > 0 && !list.includes(typed);
  row.replaceChildren();
  row.hidden = list.length === 0 && !canSave;

  for (const text of list) {
    const chip = el("span", "prompt-chip");
    const use = button(text.length > LABEL ? text.slice(0, LABEL) + "..." : text, () => {
      $("message").value = text;
      $("message").focus();
      refreshPrompts();
    });
    use.title = text;
    const remove = button("×", () => save(load().filter((kept) => kept !== text)), "remove");
    remove.title = "Forget this prompt";
    remove.setAttribute("aria-label", "Forget the prompt: " + text);
    chip.append(use, remove);
    row.append(chip);
  }

  if (canSave) {
    const add = button("+ Save this prompt", () => save([typed, ...load()]), "save-prompt");
    add.title = "Keep what you typed, to use again later";
    row.append(add);
  }
}

export function wirePrompts() {
  $("message").addEventListener("input", refreshPrompts);
  refreshPrompts();
}
