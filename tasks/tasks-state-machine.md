---
status: READY
---

Implement the task status state machine and transition rules.

**Transitions**:
- `PLANNING` → skipped by `sweep`, user-managed
- `READY` → `PROGRESSING` (by `skulk sweep` when agent is spawned)
- `PROGRESSING` → `REVIEW` (by `skulk ship` when PR is opened)
- `REVIEW` → `DONE` (by `skulk task update` when PR is merged)

Reject invalid transitions (e.g., `DONE` → `READY` without an explicit reset command).

Depends on: `tasks-format` (parser/writer).
