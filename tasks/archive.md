---
status: DONE
implemented: 18d120c
---

Add `skulk archive <name>` — kill the agent's tmux session but keep the worktree and branch intact.

Non-destructive alternative to `destroy`. Lets users stop an agent that's done (or off the rails) without losing its work.

Pairs with the `restart` task.
