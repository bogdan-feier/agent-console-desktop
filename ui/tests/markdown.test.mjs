// What the page does with what an agent writes.
//
// The first group matters most: agent text is never trusted. It can repeat
// something from a web page or a file, so anything that looks like HTML has to
// come out as text, not as tags.

import { test } from "node:test";
import assert from "node:assert/strict";

import { render, escapeHtml } from "../parts/markdown.js";

test("html in an agent's text is shown, not run", () => {
  const rendered = render('Try <script>alert("x")</script> or <img onerror=boom>');
  assert.ok(!rendered.includes("<script"), rendered);
  assert.ok(!rendered.includes("<img"), rendered);
  assert.ok(rendered.includes("&lt;script&gt;"));
});

test("html inside a code block is shown too", () => {
  const rendered = render("```html\n<b>hi</b>\n```");
  assert.ok(rendered.startsWith("<pre"), rendered);
  assert.ok(rendered.includes("&lt;b&gt;hi&lt;/b&gt;"));
  assert.ok(!rendered.includes("<b>"));
});

test("quotes and ampersands survive as themselves", () => {
  assert.equal(escapeHtml('a & b "c" <d>'), "a &amp; b &quot;c&quot; &lt;d&gt;");
});

test("a fence keeps its language and its exact contents", () => {
  const rendered = render("```python\ndef f(x):\n    return x * 2\n```");
  assert.ok(rendered.includes('data-language="python"'));
  assert.ok(rendered.includes("return x * 2"));
  assert.ok(!rendered.includes("<em>"), "no * formatting inside code");
});

test("lists, headings and emphasis", () => {
  const rendered = render("# Title\n\n- one\n- two\n\n1. first\n2. second\n\n**bold** and *slanted*");
  assert.ok(rendered.includes("<h4>Title</h4>"), rendered);
  assert.ok(rendered.includes("<ul><li>one</li><li>two</li></ul>"));
  assert.ok(rendered.includes("<ol><li>first</li><li>second</li></ol>"));
  assert.ok(rendered.includes("<strong>bold</strong>"));
  assert.ok(rendered.includes("<em>slanted</em>"));
});

test("inline code keeps its stars and its angle brackets", () => {
  const rendered = render("use `a * b` and `<T>`");
  assert.ok(rendered.includes("<code>a * b</code>"), rendered);
  assert.ok(rendered.includes("<code>&lt;T&gt;</code>"));
});

test("a link shows where it goes instead of being clickable", () => {
  const rendered = render("see [the docs](https://example.com/x)");
  assert.ok(rendered.includes("the docs (https://example.com/x)"));
  assert.ok(!rendered.includes("<a "), "nothing an agent writes becomes a link");
});

test("ordinary prose comes out as a paragraph, and empty text as nothing", () => {
  assert.equal(render("just a line"), "<p>just a line</p>");
  assert.equal(render(""), "");
});

test("an unclosed fence does not swallow the rest", () => {
  const rendered = render("before\n```\nstill code");
  assert.ok(rendered.includes("<p>before</p>"));
  assert.ok(rendered.includes("still code"));
});

test("numbered steps keep their numbers across blank lines", () => {
  // How planners write task instructions: a blank line between steps.
  const rendered = render("1. Add the column.\n\n2. Add the route.\n\n3. Add the tests.");
  assert.ok(rendered.startsWith("<ol><li>Add the column."), rendered);
  assert.ok(rendered.includes('<ol start="2"><li>Add the route.'), rendered);
  assert.ok(rendered.includes('<ol start="3"><li>Add the tests.'), rendered);
});
