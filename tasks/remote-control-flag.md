---
status: DONE
implemented: 67a9384
---

Make Claude Code's `--remote-control` flag **opt-in** on `skulk new` (currently always on).

**Motivation**:
- `--remote-control` powers the Claude Code mobile app / web UI for a session — useful when you want to drive an agent from your phone
- Skulk's own CLI commands (`connect`, `logs`, `send`, `disconnect`) all go through tmux directly and don't need it
- Always-on `--remote-control` triggers the upstream idle-death bug ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)), undermining the "spin up and come back later" workflow
- Opt-in keeps the mobile-app feature available for users who want it, while making the default behavior robust

**Change**:
- `skulk new <name> [prompt]` — launches claude **without** `--remote-control`
- `skulk new <name> --remote-control [prompt]` — launches claude **with** `--remote-control` (mobile app accessible)

**Touches**:
- `src/main.rs` — add `--remote-control` flag to `Commands::New`
- `src/commands/new.rs` — conditionally include the flag in the launch command
- README — document the flag and the mobile-app use case

**Related**: collapses scope of `remote-control-idle-death` task — the bug becomes a documented limitation of the opt-in flag rather than a workflow blocker.
