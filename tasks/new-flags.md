---
status: READY
---

Add `--model <name>` and `--claude-args "..."` flags to `skulk new` — pass-through to the Claude Code launch command.

Lets users spin up agents with different models (Opus vs Sonnet) or arbitrary Claude flags per task.

Also: extend task file frontmatter to support these fields (`model:`, `claude_args:`) so task-defined agents can specify their own.
