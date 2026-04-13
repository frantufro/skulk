---
status: READY
---

Add `skulk git-log <name>` — run `git log <default_branch>..{session_prefix}<name> --oneline` on the remote.

Named `git-log` (not `log`) to avoid collision with the existing `logs` command, which shows tmux pane output.
