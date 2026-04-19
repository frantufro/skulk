---
status: READY
---

Add `skulk replay <name>` — re-run an agent's original prompt against a fresh worktree.

Creates a new agent with the same prompt that was used to create `<name>`. Useful for retrying a task with a different model, benchmarking, or getting a second opinion on the same problem.

**Open design questions**:
- Where is the original prompt stored? Options: (a) skulk writes it to a metadata file in the worktree at creation time, (b) skulk keeps a local state dir (`.skulk/agents/<name>/prompt.txt`)
- New agent name: auto-derive (e.g. `auth-refactor-2`) or require explicit `--as <new-name>`?
- Should `--model` be passable to override the original model?

**Depends on**: a prompt-storage mechanism that doesn't exist yet — `skulk new` currently sends the prompt via tmux and doesn't persist it. This task includes adding that storage.

**Touches**:
- `src/commands/new.rs` — persist prompt to metadata file at creation time
- `src/main.rs` — add `Commands::Replay`
- `src/commands/replay.rs` — new module: read stored prompt, call `cmd_new` with it
