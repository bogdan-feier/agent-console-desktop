// What the page currently knows. One object so that any part can read it and
// the parts that change it are easy to find (search for "app.").
//
// It holds no DOM and no logic beyond the few questions that are asked from
// several places at once.

import { BUSY_STATES, REST_STATES } from "./words.js";

export const app = {
  settings: null,   // the settings as the Rust side last confirmed them
  defaults: null,   // the built-in defaults, for "Reset everything"
  limits: null,     // default turn limits and allowed ranges, for the Settings window
  folder: "",       // the project folder chosen in the top bar
  runId: null,      // the run this window is showing
  runFolder: null,  // the folder that run was started with
  runState: null,
  roundOpen: false, // true while a round's changes wait to be applied or discarded
  lastStage: 0,     // how far the round got, kept when it is stopped
  verdict: null,    // the reviewer's latest verdict in the open round
  readOnly: false,  // true when showing a finished run from an earlier launch
  nextSeq: 0,       // sequence number of the next event we expect for this run
  replaying: false,
  /// The open round came from Quick: one worker, and nobody reviewed it.
  quickRound: false,
  /// Each worker's task title for the open round, as the round event gave them.
  taskTitles: [], // true while an older run's events are being shown again
  cost: 0,          // API-equivalent dollars the agents of this run have used
  tokens: 0,        // tokens the agents of this run have used, all kinds together
  /// The same two for the current round: everything since the last round closed, so
  /// the planning that led to a round counts towards it.
  roundCost: 0,
  roundTokens: 0,
  /// Set when a round closes. The round's numbers stay on screen until the next
  /// agent starts using tokens, then count from zero.
  roundClosed: false,
};

/// agent id -> { id, role, label, number, card?, turn?, lastTurn?, hasWorked }
export const agents = {};

export const isBusy = () => BUSY_STATES.includes(app.runState);
export const isAtRest = () => app.runId !== null && !app.readOnly && REST_STATES.includes(app.runState);

/// The dispatcher and the planner talk to you in the conversation; everyone else
/// has a card of their own on the right.
export const isChat = (agent) => agent.role === "dispatcher" || agent.role === "planner";

/// "opus" plus "high" -> "opus, high effort". What a card shows before the agent
/// has run and told us the exact model it resolved to.
export function modelLabel(role) {
  const chosen = app.settings[role === "resolver" ? "worker" : role];
  if (!chosen) return "";
  return chosen.effort ? chosen.model + ", " + chosen.effort + " effort" : chosen.model;
}
