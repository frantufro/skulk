---
status: READY
---

Generate and distribute shell completions for bash, zsh, and fish.

Clap supports this natively via `clap_complete`. Small effort, big quality-of-life win — tab-completing agent names and flags.

**Approach**:
- `skulk completions <shell>` subcommand that prints the completion script to stdout (user sources it in their shell config)
- Or: generate at build time and include in the release archive

**Static completions** (commands, flags) work out of the box with `clap_complete`. **Dynamic completions** (agent names) would require a custom completer that calls `skulk list` — nice to have but not required for v1.

**Touches**:
- `Cargo.toml` — add `clap_complete` dependency
- `src/main.rs` — add `Commands::Completions` variant (or build script generation)
- README — document how to install completions per shell
