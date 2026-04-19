---
status: DONE
implemented: 0050e20
---

Add output-mode flags to `skulk diff <name>` — passthrough to `git diff`.

**Flags**:
- `--stat` — summary of files changed, insertions, deletions
- `--name-only` — just the list of changed file paths

These thread through the `diff_command` builder and the `clap` definition in `main.rs`. Kept out of the initial `skulk diff` commit to keep that commit single-purpose.

**Touches**:
- `src/main.rs` — extend `Commands::Diff` with optional flags
- `src/commands/interact.rs` — `diff_command` takes flags and appends them to the `git diff` invocation; new tests for each flag combination

**Depends on**: `diff` (base command must exist first).
