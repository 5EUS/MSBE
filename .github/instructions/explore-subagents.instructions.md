---
description: "Use when delegating repository exploration, code search, or read-only investigation to an Explore subagent. Keeps exploration short, evidence-based, and directional."
name: "Explore Subagent Guidance"
---
# Explore Subagent Guidance

Use the `Explore` subagent to locate the next relevant code surface, not to establish the full repository state.

- Request `quick` thoroughness by default. Use `medium` only when the task cannot be narrowed by one focused search or local read; do not request `thorough` unless explicitly needed.
- Give the subagent a narrow question, likely anchors, and the exact facts needed to select the next file, symbol, or test.
- Ask for a compact report: the best next anchor, supporting file paths and symbols, one-sentence rationale, and any unresolved ambiguity. Do not ask for broad architecture summaries, inventories, or claims that a search was exhaustive.
- Treat the report as pointers, not proof. Verify its key claim with a targeted local read before editing or making repository-wide assertions.
- When the report is inconclusive, make one local read or targeted search from its best anchor before launching another Explore subagent.

Example delegation:

> Quick: Locate the function that decides retry eligibility for failed downloads. Return its path and symbol, the closest focused test, and one sentence on the controlling condition. Do not summarize adjacent subsystems.
