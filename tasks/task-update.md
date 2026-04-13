---
status: READY
---

Add `skulk task update` — refresh task statuses from external state (GitHub PRs).

For each task in `REVIEW`, query `gh pr view <pr-number> --json state` and flip to `DONE` when the PR is merged.

Depends on: `tasks-format`, `tasks-state-machine`, `ship-task-integration` (which records the PR number).
