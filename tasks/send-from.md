---
status: READY
---

Add `--from <file>` flag to `skulk send` — read a local text file and deliver its contents as the prompt.

Mirrors the `--from` flag on `skulk new`. Useful for sending multi-line follow-up instructions, code snippets, or structured prompts to a running agent without shell-quoting gymnastics.

**Behavior**:
- `skulk send <name> --from <file>` — read file locally, send contents as prompt
- Mutually exclusive with the positional `prompt` argument
- File is read on the local machine, contents shipped over SSH via tmux send-keys (same path as the existing `cmd_send`)

**Touches**:
- `src/main.rs` — add `--from` flag to `Commands::Send`, make `prompt` optional with `required_unless_present`
- `src/commands/interact.rs` — `cmd_send` accepts the resolved prompt string (caller reads file); tests for the new flag
