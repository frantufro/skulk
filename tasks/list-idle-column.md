---
status: READY
---

Enrich `skulk list` output with an IDLE column.

Each agent should show one of: `working` / `idle` / `stopped`, based on the Stop-hook state file.

Visual cue for "which agents have something ready to review."

Depends on: `wait` task (Stop hook + state file infrastructure must exist first).
