---
status: DONE
implemented: b03ba11
---

Add `skulk restart <name>` — spin up a fresh tmux session in an existing worktree (archived, crashed, or stopped agent).

**Open design**: does it start a fresh Claude with empty context, or attempt to resume previous state (e.g., via `claude --resume` or similar)?

Depends conceptually on: `archive` (target use case) and the stopped-session tracking that already exists in inventory/list.
