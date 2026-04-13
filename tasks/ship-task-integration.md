---
status: READY
---

Wire `skulk ship` into the tasks-system state machine.

When shipping an agent whose name matches a task file in `tasks/`:
- Flip the task status `PROGRESSING` → `REVIEW`
- Record the PR number in frontmatter (e.g., `pr: 123`)

Depends on: `ship` (base command), `tasks-format`, `tasks-state-machine`.
