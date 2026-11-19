// Turns the Markdown agents write into HTML.
//
// The one rule that matters: escape the text FIRST, then add our own tags. The
// text comes from a model, which may be repeating something it read in a file
// or on a web page, so it is never trusted. After escaping, the only "<" left
// in the string are the ones this file puts there.

const ESCAPES = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" };

export function escapeHtml(text) {
  return String(text).replace(/[&<>"]/g, (character) => ESCAPES[character]);
}

/// Bold, italic, inline code and links, inside one line of already-escaped text.
function inline(text) {
  // Inline code is taken out first so that `**` inside it stays literal, and put
  // back at the end. \u0000 cannot appear in the text: escapeHtml left it alone,
  // but no Markdown produces it either.
  const code = [];
  let out = text.replace(/`([^`]+)`/g, (whole, inner) => {
    code.push(inner);
    return "\u0000" + (code.length - 1) + "\u0000";
  });
  out = out
    .replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, "$1 ($2)") // links: show the address, don't link it
    .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[\s(])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/(^|[\s(])_([^_\n]+)_/g, "$1<em>$2</em>");
  return out.replace(/\u0000(\d+)\u0000/g, (whole, index) => "<code>" + code[Number(index)] + "</code>");
}

const BULLET = /^\s*[-*+]\s+(.*)$/;
const NUMBER = /^\s*(\d+)[.)]\s+(.*)$/;
const HEADING = /^(#{1,6})\s+(.*)$/;

/// Everything that is not a fenced code block: headings, lists, paragraphs.
function prose(text) {
  const lines = escapeHtml(text).split("\n");
  const html = [];
  let list = null;      // "ul" or "ol" while one is open
  let paragraph = [];

  const endParagraph = () => {
    if (paragraph.length) html.push("<p>" + inline(paragraph.join("\n")) + "</p>");
    paragraph = [];
  };
  const endList = () => {
    if (list) html.push("</" + list + ">");
    list = null;
  };
  // `start` keeps the agent's own numbers: planners put a blank line between steps,
  // which closes the list, and a list opened again would count from 1 each time.
  const openList = (kind, start) => {
    if (list !== kind) {
      endList();
      html.push(start > 1 ? '<ol start="' + start + '">' : "<" + kind + ">");
      list = kind;
    }
  };

  for (const line of lines) {
    const heading = line.match(HEADING);
    const bullet = line.match(BULLET);
    const numbered = line.match(NUMBER);
    if (line.trim() === "") {
      endParagraph();
      endList();
    } else if (heading) {
      endParagraph();
      endList();
      // Agent headings sit inside a reply, so they start small: # is an h4.
      const level = Math.min(6, heading[1].length + 3);
      html.push("<h" + level + ">" + inline(heading[2]) + "</h" + level + ">");
    } else if (bullet || numbered) {
      endParagraph();
      if (bullet) openList("ul");
      else openList("ol", Number(numbered[1]));
      html.push("<li>" + inline(bullet ? bullet[1] : numbered[2]) + "</li>");
    } else {
      endList();
      paragraph.push(line);
    }
  }
  endParagraph();
  endList();
  return html.join("");
}

/// Markdown to HTML. Text between ``` fences is kept exactly as written.
export function render(text) {
  // Splitting on ``` puts the fenced blocks at the odd positions.
  return String(text)
    .split("```")
    .map((part, index) => {
      if (index % 2 === 0) return prose(part);
      const newline = part.indexOf("\n");
      // The first line of a fence is the language, which we only use as a label.
      const language = newline === -1 ? "" : part.slice(0, newline).trim();
      const code = newline === -1 ? part : part.slice(newline + 1);
      const label = language ? ' data-language="' + escapeHtml(language) + '"' : "";
      return "<pre" + label + "><code>" + escapeHtml(code.replace(/\n$/, "")) + "</code></pre>";
    })
    .join("");
}

/// An element whose content is `text` rendered as Markdown.
export function rich(tag, text, className) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.innerHTML = render(text);
  return node;
}
