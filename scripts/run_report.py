#!/usr/bin/env python3
"""Print a Markdown report of one saved run: every message you sent, and what it cost.

    python3 scripts/run_report.py                  # the newest run
    python3 scripts/run_report.py ~/equipment-svc  # the newest run in that project
    python3 scripts/run_report.py 756bb185b558     # one run by its ID

Runs saved before the cost fix (4310f9c) recorded a resumed session's running total
instead of what each call added. Those are recognised by their state file having no
`session_costs`, and corrected here by counting only what each total added.
"""

from __future__ import annotations

import json
import sys
from collections import defaultdict
from pathlib import Path

RUNS = Path.home() / "Library/Application Support/dev.agentconsole.desktop/runs"
TOKEN_KINDS = ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")


def load(path: Path) -> tuple[dict, list[dict]]:
    header, events = {}, []
    for line in path.read_text().splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue  # a line cut short by a crash
        if "seq" in item:
            events.append(item)
        else:
            header = item
    return header, events


def find_run(arg: str | None) -> Path:
    files = sorted(RUNS.glob("*.jsonl"), key=lambda path: path.stat().st_mtime, reverse=True)
    if not arg:
        return files[0]
    if (RUNS / f"{arg}.jsonl").exists():
        return RUNS / f"{arg}.jsonl"
    folder = str(Path(arg).expanduser().resolve())
    for path in files:
        if load(path)[0].get("project_dir") == folder:
            return path
    sys.exit(f"No run found for {arg}")


def new_step(prompt: str, agent: str | None) -> dict:
    return {
        "prompt": prompt,
        "to": agent,
        "cost": defaultdict(float),
        "turns": defaultdict(int),
        "tokens": defaultdict(int),
        "models": {},
        "tasks": [],
        "quick": False,
        "verdicts": [],
        "checks": [],
        "refused": [],
        "tools": 0,
    }


def steps_of(events: list[dict], running_totals: bool) -> list[dict]:
    steps: list[dict] = []
    last_total: dict[str, float] = {}
    for event in events:
        kind, agent = event["type"], event.get("agent")
        if kind == "user_message":
            # The dispatcher passes a message on to the planner: one step, not two.
            if not steps or steps[-1]["prompt"] != event["text"]:
                steps.append(new_step(event["text"], agent))
            continue
        if not steps:
            continue
        step = steps[-1]
        if kind == "result":
            cost = event.get("cost_usd") or 0.0
            if running_totals:
                before = last_total.get(agent)
                last_total[agent] = cost
                cost = cost - before if before is not None and cost >= before else cost
            step["cost"][agent] += cost
            step["turns"][agent] += event.get("turns") or 0
            for name in TOKEN_KINDS:
                step["tokens"][name] += (event.get("tokens") or {}).get(name, 0)
        elif kind == "agent_model":
            step["models"][agent] = event["model"]
        elif kind == "plan":
            step["tasks"] = [task.get("title", "") for task in event["tasks"]]
        elif kind == "round" and event.get("open"):
            step["quick"] = step["quick"] or event.get("quick", False)
        elif kind == "review":
            step["verdicts"].append(event["verdict"])
        elif kind == "checks":
            step["checks"].append("passed" if event["passed"] else "failed")
        elif kind == "tool_error" and event.get("allow_rule"):
            step["refused"].append(event["allow_rule"])
        elif kind == "tool":
            step["tools"] += 1
    return steps


def route(step: dict) -> str:
    if step["to"] and step["to"] not in ("dispatcher", "planner"):
        return f"Message to {step['to']}"
    if step["quick"]:
        return "Quick change"
    if step["tasks"]:
        return f"Plan, {len(step['tasks'])} task(s)"
    if set(step["cost"]) <= {"dispatcher"}:
        return "Ask (answered by the dispatcher)"
    return "Brainstorm / answer"


def report(path: Path) -> str:
    header, events = load(path)
    state_file = path.with_suffix(".state.json")
    running_totals = not (state_file.exists() and "session_costs" in json.loads(state_file.read_text()))
    steps = steps_of(events, running_totals)
    lines = [f"# Run {path.stem}", "", f"Project: `{header.get('project_dir')}`"]
    if models := header.get("models"):
        roles = ", ".join(
            f"{role} {spec['model']}" + (f"/{spec['effort']}" if spec.get("effort") else "")
            for role, spec in models.items()
            if isinstance(spec, dict)
        )
        lines.append(f"Team: preset {models.get('preset')}, {models.get('workers')} workers; {roles}")
    if running_totals:
        lines.append("_Saved before the cost fix: costs corrected from running totals._")
    total = 0.0
    for number, step in enumerate(steps, 1):
        cost = sum(step["cost"].values())
        total += cost
        tokens = step["tokens"]
        lines += ["", f"## Step {number}: {route(step)} — ${cost:.2f}", "", "> " + step["prompt"].replace("\n", "\n> "), ""]
        if step["tasks"]:
            lines += [f"- Tasks: " + "; ".join(step["tasks"])]
        if step["checks"]:
            lines += [f"- Checks: {', '.join(step['checks'])}"]
        if step["verdicts"]:
            lines += [f"- Reviews: {', '.join(step['verdicts'])}"]
        lines += [f"- Tool calls: {step['tools']}, refused: {len(step['refused'])}"
                  + (f" ({', '.join(sorted(set(step['refused'])))})" if step["refused"] else "")]
        lines += [
            f"- Tokens: {tokens['input_tokens']:,} input, {tokens['output_tokens']:,} output, "
            f"{tokens['cache_read_input_tokens']:,} cache read, {tokens['cache_creation_input_tokens']:,} cache write",
            "",
            "| Agent | Model | Turns | Cost |",
            "|---|---|---|---|",
        ]
        for agent, agent_cost in sorted(step["cost"].items(), key=lambda item: -item[1]):
            lines.append(f"| {agent} | {step['models'].get(agent, '')} | {step['turns'][agent]} | ${agent_cost:.3f} |")
    lines += ["", f"**Total: ${total:.2f} over {len(steps)} step(s)**"]
    return "\n".join(lines)


if __name__ == "__main__":
    print(report(find_run(sys.argv[1] if len(sys.argv) > 1 else None)))
