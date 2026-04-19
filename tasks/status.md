---
status: READY
---

Add `skulk status <name>` — single-agent detail view.

`skulk list` is the fleet overview; `status` is the deep look at one agent. Single SSH roundtrip gathers everything, formatted for quick scanning.

**Output** (draft — tune at implementation time):
```
Agent:    auth-refactor
Status:   idle
Uptime:   47m
Branch:   skulk-auth-refactor
Commits:  3 ahead of main
Files:    5 changed (+120 -34)
Worktree: ~/myproject-worktrees/skulk-auth-refactor
```

**Data sources** (all available on the remote):
- Idle/working/stopped — same state file used by `list` and `wait`
- Uptime — tmux session creation time (already gathered by inventory)
- Commits ahead — `git rev-list --count <default_branch>..<branch>`
- Files changed summary — `git diff --stat <default_branch>...<branch> | tail -1`
- Worktree path — inventory

**Touches**:
- `src/main.rs` — add `Commands::Status` variant
- `src/commands/status.rs` — new module, gather + format, co-located tests
- `src/commands/mod.rs` — add `pub(crate) mod status`
