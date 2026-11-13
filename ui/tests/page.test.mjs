// The page and its modules have to agree with each other.
//
// `node --check` only finds syntax errors. The mistakes that actually happen are
// an element renamed in index.html but not in the code, or a module importing
// something its neighbour no longer exports. Both show up as a dead window with
// an error nobody sees, so they are worth catching here.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const ui = join(dirname(fileURLToPath(import.meta.url)), "..");
const html = readFileSync(join(ui, "index.html"), "utf8");

const scripts = ["app.js", ...readdirSync(join(ui, "parts")).map((name) => join("parts", name))]
  .filter((name) => name.endsWith(".js"))
  .map((name) => ({ name, text: readFileSync(join(ui, name), "utf8") }));

const matchAll = (text, pattern) => [...text.matchAll(pattern)].map((found) => found[1]);

test("every element the code asks for is in the page", () => {
  const inPage = new Set(matchAll(html, /\sid="([^"]+)"/g));
  for (const script of scripts) {
    for (const id of matchAll(script.text, /\$\("([^"]+)"\)/g)) {
      assert.ok(inPage.has(id), `${script.name} looks for #${id}, which index.html does not have`);
    }
  }
});

test("every id in the page is used once", () => {
  const ids = matchAll(html, /\sid="([^"]+)"/g);
  assert.equal(new Set(ids).size, ids.length, "two elements share an id");
});

test("every import names something its module exports", () => {
  const exportsOf = new Map(
    scripts.map((script) => [
      script.name.replace("parts/", ""),
      new Set([
        ...matchAll(script.text, /export (?:async )?function ([\w$]+)/g),
        ...matchAll(script.text, /export const ([\w$]+)/g),
        // export const { invoke } = ...
        ...matchAll(script.text, /export const \{([^}]+)\} =/g).flatMap((list) =>
          list.split(",").map((name) => name.trim())),
        ...matchAll(script.text, /export \{ ([^}]+) \}/g).flatMap((list) =>
          list.split(",").map((name) => name.trim())),
      ]),
    ]),
  );
  for (const script of scripts) {
    const imports = [...script.text.matchAll(/import \{([^}]+)\} from "\.\/(?:parts\/)?([\w.]+)"/g)];
    for (const [, names, from] of imports) {
      const available = exportsOf.get(from);
      if (!available) continue; // not one of ours
      for (const name of names.split(",").map((one) => one.trim()).filter(Boolean)) {
        assert.ok(available.has(name), `${script.name} imports ${name} from ${from}, which does not export it`);
      }
    }
  }
});

test("the page loads its code as one module, so the imports work at all", () => {
  assert.match(html, /<script type="module" src="app\.js"><\/script>/);
});

test("nothing is loaded from the internet", () => {
  // The app has no network access of its own and must not depend on one.
  assert.ok(!/src="https?:|href="https?:/.test(html), "index.html reaches outside the app");
});
