---
status: READY
---

Add `skulk doctor` — health check command that verifies the runtime environment is correctly set up.

Useful for debugging setup issues without re-running `skulk init`. Runs checks and reports pass/fail for each.

**Checks**:
- SSH connectivity to configured host
- tmux installed and reachable
- `claude` binary present on remote
- `gh` CLI installed and authenticated (warn, not fail — only needed for `--github` and `ship`)
- Base clone exists at `base_path`
- `worktree_base` directory exists
- `.skulk/config.toml` is valid and loadable

**Output** (draft):
```
Config:       .skulk/config.toml        OK
SSH:          dev-server                 OK
tmux:         3.3a                       OK
claude:       installed                  OK
gh:           authenticated              OK
Base clone:   ~/myproject                OK
Worktree dir: ~/myproject-worktrees      OK
```

**Touches**:
- `src/main.rs` — add `Commands::Doctor`
- `src/commands/doctor.rs` — new module, run checks in sequence, co-located tests
- `src/commands/mod.rs` — add `pub(crate) mod doctor`
