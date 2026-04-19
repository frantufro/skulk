---
status: DONE
implemented: c9173de
---

Add `skulk push <name>` — push the `{session_prefix}<name>` branch to `origin` via SSH.

One-line wrapper around `git push -u origin <branch>`. Handle: push failure, branch not found, no upstream remote configured.
