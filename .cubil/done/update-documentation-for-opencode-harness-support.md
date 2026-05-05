---
created: 2026-05-04
---

# Update documentation for OpenCode harness support

Update README.md and any other user-facing docs to cover the new `harness` config option added in v0.4.0.

**What to document**:
- `harness` field in `.skulk/config.toml` (default: `claude`, alternative: `opencode`)
- How to install OpenCode on the remote server
- The `skulk wait` limitation with OpenCode (no UserPromptSubmit equivalent — may return early right after sending a prompt)
- `--model` flag requires `provider/model` format when using OpenCode (e.g. `anthropic/claude-opus-4-7`)
- `--remote-control` is Claude-only and ignored for OpenCode

**Touches**:
- `README.md` — add an Alternative Harnesses section or expand the config reference
