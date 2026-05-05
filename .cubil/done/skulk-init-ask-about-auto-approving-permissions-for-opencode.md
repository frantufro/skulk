---
created: 2026-05-04
---

# skulk init: ask about auto-approving permissions for OpenCode

When `harness = "opencode"`, add a step to the `skulk init` wizard asking whether to auto-approve all tool permissions.

**Why**: OpenCode's TUI prompts for tool approval by default. For headless skulk agents there's no human present, so permissions must be pre-approved via `opencode.json` with `{"permission": "allow"}`.

**Deliverable**:
- Add a new wizard step in `src/commands/init.rs` that only appears when `harness = "opencode"`:
  > Auto-approve all tool permissions? (required for headless agents) [Y/n]
- Store the answer as `auto_approve_permissions: bool` in `WizardAnswers`
- In `agent_create_worktree_command` (`src/commands/new.rs`), write `opencode.json` with `{"permission": "allow"}` to the worktree root when `auto_approve_permissions = true`
- Add `auto_approve_permissions` as an optional bool field in `Config` (`src/config.rs`), defaulting to `false`
- Add tests: wizard prompts for permission when harness=opencode, skips it for claude; worktree command writes/omits opencode.json based on the flag

**Touches**:
- `src/config.rs` — add `auto_approve_permissions: bool`
- `src/commands/init.rs` — new conditional wizard step
- `src/commands/new.rs` — write opencode.json when flag is set
