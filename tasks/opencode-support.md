---
status: READY
---

Add [OpenCode](https://opencode.ai) support as an alternative to Claude Code.

Skulk currently hard-codes `claude --dangerously-skip-permissions` as the agent
command (see `src/commands/new.rs:72`). OpenCode is an open-source, multi-model
alternative to Claude Code with a compatible terminal UI. Supporting it lets users
run skulk agents with OpenCode instead of (or alongside) Claude Code.

**Deliverable**:
- Add an optional `agent_command` field to `Config` in `src/config.rs`.
  Defaults to `"claude"` when absent so existing configs keep working.
- In `build_launch_sequence` (`src/commands/new.rs`), use `cfg.agent_command`
  instead of the hard-coded `"claude"` string.
- Drop `--dangerously-skip-permissions` when `agent_command != "claude"` — it is
  Claude-specific and will cause OpenCode to error.
- The `--model` flag format differs between tools. Add a `model_flag_format` or
  derive it from `agent_command`:
  - `claude`: `--model <value>`
  - `opencode`: `--model <value>` (same, confirm on release)
- Update `skulk doctor` (`src/commands/init.rs`) to check for the configured
  `agent_command` binary rather than hard-coding `claude`.
- Add tests for both `claude` and `opencode` launch sequences (MockSsh).

**Config example**:
```toml
host = "your-server"
session_prefix = "skulk-"
base_path = "~/your-project"
worktree_base = "~/your-project-worktrees"
agent_command = "opencode"   # defaults to "claude"
```

**Touches**:
- `src/config.rs` — add `agent_command: Option<String>`, default to `"claude"`
- `src/commands/new.rs` — use `cfg.agent_command`, conditional permission flag
- `src/commands/init.rs` — check configured binary in doctor
- `src/commands/restart.rs` — same launch path, picks up change automatically
