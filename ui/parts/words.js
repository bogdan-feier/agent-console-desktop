// Every piece of fixed text and every lookup table the page uses, in one place,
// so wording can be changed without reading the code that shows it.

export const STATUS_WORDS = {
  idle: "Waiting",
  running: "Working",
  done: "Finished",
  error: "Stopped early",
  stopped: "Stopped",
};

export const STATE_WORDS = {
  ready: "Ready for your next message",
  chatting: "An agent is replying",
  planning: "The planner is writing a plan",
  awaiting_approval: "Waiting for you to approve the plan",
  building: "Workers are building",
  reviewing: "The reviewer is checking",
  done: "Reviewed: apply, discard, or send work back",
  failed: "Stopped on an error",
  stopped: "Stopped",
};

export const CARD_EMPTY = {
  worker: "Gets a task when you approve a plan, and works in its own copy of the project.",
  reviewer: "Checks all the workers' changes together when they finish. Then you decide what happens to them.",
  resolver: "Only runs when two workers changed the same lines, to merge their work.",
};

export const PLACEHOLDER = {
  dispatcher: "Ask a question or describe a change",
  brainstorm: "Think an idea through with the planner",
  plan: "Anything the plan should take into account (optional)",
  planFirst: "Describe the change you want planned",
  quick: "Describe one small change for a single worker to make",
};

/// Which lane's colour an agent's output is drawn in.
export const HUE = {
  dispatcher: "plan-hue",
  planner: "plan-hue",
  worker: "build-hue",
  reviewer: "review-hue",
  resolver: "build-hue",
};

export const BUSY_STATES = ["chatting", "planning", "building", "reviewing"];
export const REST_STATES = ["ready", "awaiting_approval", "done", "stopped", "failed"];

export const AUTH_WORDS = {
  claude_login: "Signed in with Claude Code",
  api_key: "Billing: API key",
};

export const MODEL_CHOICES = [["opus", "Opus"], ["sonnet", "Sonnet"], ["haiku", "Haiku"]];

export const EFFORT_CHOICES = [
  ["", "Default"],
  ["low", "Low"],
  ["medium", "Medium"],
  ["high", "High"],
  ["xhigh", "Extra high"],
  ["max", "Max"],
];

export const PRESET_WORDS = {
  lean: ["Lean", "Least usage: Sonnet plans, low effort for workers and reviewer"],
  balanced: ["Balanced", "The default: Opus plans, Sonnet builds and reviews, medium effort"],
  thorough: ["Thorough", "Most careful and most usage: high effort, Opus also reviews"],
};

/// Offered in an empty conversation, as [mode, text].
export const EXAMPLES = [
  ["dispatcher", "How is this project organised?"],
  ["dispatcher", "Add input validation to the signup form, with tests"],
  ["brainstorm", "What would make the test suite faster?"],
];

/// 45210 -> "45.2k". Used for token counts, which get long.
export function count(tokens) {
  if (tokens < 1000) return String(tokens);
  if (tokens < 1000000) return (tokens / 1000).toFixed(1) + "k";
  return (tokens / 1000000).toFixed(2) + "M";
}

/// 1 -> "1 task", 2 -> "2 tasks".
export function plural(number, word) {
  return number + " " + word + (number === 1 ? "" : "s");
}
