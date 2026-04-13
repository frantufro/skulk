---
status: READY
---

Investigate and fix/work-around the `--remote-control` idle-death bug.

Claude Code `--remote-control` sessions die after ~20 min of inactivity, which undermines skulk's entire "spin up and come back later" workflow.

Upstream: https://github.com/anthropics/claude-code/issues/32982

**Possible approaches**:
- Keepalive loop that sends periodic no-op activity to the session
- Alternate launch mode (not `--remote-control`)
- Wait for upstream fix and document the limitation

Start with an investigation writeup; then propose a fix.
