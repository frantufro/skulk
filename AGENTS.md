# AGENTS.md

## Build & Verify
- `cargo clippy -- -D warnings -W clippy::pedantic` — lint, zero warnings required (non-obvious flags)
- `cargo test` — full suite, all pass before commit
- `cargo check` — prefer over `cargo build` during iteration

## Architecture
- `src/io.rs` — system boundary (real SSH, stdin), excluded from coverage via `--ignore-filename-regex 'io\.rs$'`
- `src/ssh.rs` — `Ssh` trait, injectable for testing with `MockSsh` (`src/testutil.rs`)
- `src/commands/` — one module per command, co-located `#[cfg(test)]` tests
- `src/agent_ref.rs` — canonical agent name/branch/worktree/session resolver
- `src/timings.rs` — injectable clock and sleep for deterministic tests
- `src/prompt_source.rs` — resolve prompt from `--from`/`--github`

## Code Rules (non-default)
- No `.unwrap()`/`.expect()` in production code — use `thiserror` + `anyhow::Context` with meaningful messages on every `?`
- No `.clone()` to fix borrow checker — restructure ownership first
- `unsafe` requires `// SAFETY:` comment
- No `async` unless doing real I/O
- Default to no comments; add only when `why` is non-obvious

## Testing
- TDD: write failing test first
- Test names: `returns_error_when_<condition>()` style
- No real SSH in unit tests — use `MockSsh`

## Commits & Branches
- Never mix structural (`refactor:`) and behavioral (`feat:`/`fix:`) changes in one commit
- Work in current branch — do not create new branches (run `git branch --show-current` to confirm)

## Agent Status Checks
- Always spawn a subagent to check status: run `skulk list` + `skulk logs <name> --lines 5` per agent, return concise summary table

## Runtime Config
- `.skulk/config.toml` (searched upward from cwd)
- `host = "localhost"` for local testing
