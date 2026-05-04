---
created: 2026-05-04
---

# Fix OpenCode harness — permissions, model flag, remote-control

The current harness implementation has three bugs when harness = 'opencode':

1. **--dangerously-skip-permissions** is passed to the TUI but only exists on `opencode run`. Fix: remove the flag from the TUI launch command for opencode; instead write `opencode.json` with `{"permission": "allow"}` to the worktree root in `agent_create_worktree_command` (alongside the existing plugin write).

2. **--model flag format**: OpenCode uses `provider/model` (e.g. `anthropic/claude-opus-4-7`) but skulk currently passes the raw value users provide (e.g. `opus`). Decide: either document that users must pass the full `provider/model` format when using opencode, or add a mapping. Simplest: document it, no code change needed, just update the `--model` help text.

3. **--remote-control** is Claude-specific and not a valid OpenCode flag. Skip it (with a warning or silently) when harness != 'claude'.

**Touches**:
- `src/commands/new.rs` — conditional --dangerously-skip-permissions (claude-only), write opencode.json for opencode harness, skip --remote-control for non-claude
- `src/main.rs` — update --model help text to note provider/model format for OpenCode
- Tests for all three fixes (MockSsh)
