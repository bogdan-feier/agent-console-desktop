// The Changes window: what a round would put into your project, and the two
// ways to accept it (staged only, or staged and committed).

import { $, el, recall, remember, showProblem } from "./dom.js";
import { app } from "./state.js";
import { invoke } from "./tauri.js";
import { plural } from "./words.js";

let files = [];

export async function openChanges() {
  const list = $("change-files");
  const patch = $("change-patch");
  list.replaceChildren(el("p", "hint", "Reading the changes..."));
  patch.replaceChildren();
  $("changes-notice").textContent = "";
  $("changes").showModal();

  try {
    const changes = await invoke("round_changes", { runId: app.runId });
    files = changes.files;
    $("commit-message").value = changes.suggested_message;
    // Whether to push is a habit, not a per-round decision, so it is remembered.
    $("push-after-commit").checked = recall("push-after-commit") === "yes";
    showFileList();
    if (files.length) showFile(0);
  } catch (error) {
    list.replaceChildren();
    $("changes-notice").textContent = typeof error === "string" ? error : String(error);
  }
}

function showFileList() {
  const list = $("change-files");
  list.replaceChildren();
  if (!files.length) {
    list.append(el("p", "hint", "No changes in this round."));
    return;
  }
  const summary = files.reduce(
    (total, file) => ({ added: total.added + file.added, removed: total.removed + file.removed }),
    { added: 0, removed: 0 },
  );
  const head = el("p", "change-summary");
  head.append(
    el("span", "", plural(files.length, "file") + " "),
    el("span", "added", "+" + summary.added),
    el("span", "removed", " -" + summary.removed),
  );
  list.append(head);

  files.forEach((file, index) => {
    const entry = el("button", "change-file");
    entry.type = "button";
    entry.append(el("span", "path", file.path));
    const counts = el("span", "counts");
    counts.append(el("span", "added", "+" + file.added), el("span", "removed", " -" + file.removed));
    entry.append(counts);
    if (file.workers.length) {
      entry.append(el("span", "by", file.workers.map((number) => "Worker " + number).join(", ")));
    }
    entry.addEventListener("click", () => showFile(index));
    list.append(entry);
  });
}

function showFile(index) {
  for (const entry of $("change-files").querySelectorAll(".change-file")) entry.classList.remove("chosen");
  $("change-files").querySelectorAll(".change-file")[index].classList.add("chosen");

  const patch = $("change-patch");
  patch.replaceChildren();
  const lines = files[index].patch.split("\n");
  for (const line of lines) {
    const kind = line.startsWith("+++") || line.startsWith("---") ? "meta"
      : line.startsWith("@@") ? "hunk"
        : line.startsWith("+") ? "add"
          : line.startsWith("-") ? "del"
            : line.startsWith("diff ") || line.startsWith("index ") ? "meta" : "ctx";
    patch.append(el("div", "line " + kind, line || " "));
  }
}

/// Apply, with or without making the commit. Both close the round.
async function apply(commit) {
  const message = $("commit-message").value.trim();
  if (commit && !message) {
    $("changes-notice").textContent = "Write a commit message first.";
    return;
  }
  const push = commit && $("push-after-commit").checked;
  remember("push-after-commit", push ? "yes" : "no");
  try {
    await invoke("apply_changes", { runId: app.runId, commitMessage: commit ? message : null, push });
    $("changes").close();
  } catch (error) {
    $("changes-notice").textContent = typeof error === "string" ? error : String(error);
  }
}

export function wireChanges() {
  $("open-changes").addEventListener("click", () => openChanges().catch(showProblem));
  $("changes-close").addEventListener("click", () => $("changes").close());
  $("apply-only").addEventListener("click", () => apply(false));
  $("apply-commit").addEventListener("click", () => apply(true));
}
