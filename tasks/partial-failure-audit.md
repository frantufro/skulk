---
status: READY
---

Audit all multi-step commands for partial-failure cleanup.

`skulk new` already rolls back the worktree if the tmux session fails to start. Other multi-step commands don't have equivalent guards:

**Commands to audit**:
- `ship` — push succeeds but `gh pr create` fails: branch is pushed with no PR. Should this be rolled back, or is "pushed but no PR" an acceptable state with a clear error?
- `restart` — tmux session creation fails after confirming the worktree exists: session may be half-created
- `destroy` / `destroy-all` — one cleanup step fails (e.g., branch delete) but others succeed: agent is partially torn down
- `new --github` — `gh` fetch succeeds but agent creation fails: no cleanup needed (fetch is read-only), but verify

**Deliverable**: for each command, either add rollback/cleanup or document why the partial state is acceptable. No speculative robustness — only fix real failure modes that leave the user stuck.

**Touches**:
- Likely `src/commands/ship.rs`, `src/commands/restart.rs`, `src/commands/destroy.rs`
- Co-located tests for each new failure path
