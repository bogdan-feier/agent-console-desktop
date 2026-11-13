// Small helpers for building and finding elements, and for the two bits of
// browser storage the page uses. No knowledge of agents or runs lives here.

export const $ = (id) => document.getElementById(id);

export function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

export function button(text, onClick, className) {
  const node = el("button", className || "", text);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
}

/// Append to a scrolling box, following along only if the reader was already at
/// the bottom. Someone who scrolled up to read is left where they are.
export function append(box, node) {
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 80;
  box.append(node);
  if (nearBottom) box.scrollTop = box.scrollHeight;
}

// --- Remembering things between launches ---
// localStorage can throw (private mode, a locked profile), and none of what we
// keep here is important enough to break the page over.

export function remember(key, value) {
  try { localStorage.setItem(key, value); } catch (error) { /* not important */ }
}

export function recall(key) {
  try { return localStorage.getItem(key); } catch (error) { return null; }
}

export function forget(key) {
  try { localStorage.removeItem(key); } catch (error) { /* not important */ }
}

/// A list kept in storage as JSON. Returns [] if it is missing or damaged.
export function recallList(key) {
  try { return JSON.parse(recall(key)) || []; } catch (error) { return []; }
}

export function rememberList(key, list) {
  remember(key, JSON.stringify(list));
}

// --- Problems ---
// Commands fail with a plain string (Rust's Err(String)); anything else is a bug
// in the page, and String() still gives something readable.

export function problemText(error) {
  return typeof error === "string" ? error : String((error && error.message) || error);
}

export function showProblem(error) {
  $("notice").textContent = problemText(error);
}

export function clearProblem() {
  $("notice").textContent = "";
}
