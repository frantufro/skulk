---
status: DONE
implemented: 233bb87
---

Add `skulk transcript <name>` — dump the full tmux scrollback to stdout (or to a file via `--output <path>`).

Essentially `logs --lines 100000` with a clearer name for the "I want the whole session for review or archive" use case.
