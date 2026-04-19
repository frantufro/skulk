---
status: DONE
implemented: cb51c8e
---

Add `skulk wait <name>` (and `--all`) — block until the agent is idle (finished current turn, ready for next input).

**Approach**:
- Install a Claude Code `Stop` hook at agent creation that writes a marker file (e.g. `~/.skulk/state/<session>`)
- Poll the marker file's mtime; when it updates after the current turn starts, the agent has finished
- Optional CPU-idle fallback: poll `ps` for the `claude` process

Requires changes to `skulk new` to also register/install the Stop hook.

**Research context**: Claude Code exposes no canonical "idle" signal. The `Stop` hook is the closest native mechanism — fires on turn-end with session metadata on stdin.
