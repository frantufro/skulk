---
status: READY
---

Add `skulk sweep` — scan `tasks/`, pick up every task in `READY` status, spawn an agent per task, flip status to `PROGRESSING`.

- Agent name = filename (without `.md`)
- Prompt = body (frontmatter stripped)
- Tasks in any other status (`PLANNING`, `PROGRESSING`, `REVIEW`, `DONE`) are skipped

Depends on: `tasks-format`, `tasks-state-machine`.
