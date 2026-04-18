# Claude Code Configuration

## Project Overview

Skulk is a CLI tool that manages remote Claude Code agents over SSH. Each agent
gets its own tmux session and git worktree on a remote server, enabling parallel
autonomous coding workflows.

## Build & Check Commands

```bash
cargo fmt                                        # Format — always, no exceptions
cargo clippy -- -D warnings -W clippy::pedantic  # Lint — zero warnings
cargo test                                       # Test — all pass before commit
cargo check                                      # Prefer over cargo build during iteration
```

## Architecture

- `src/main.rs`        — CLI definition (`Cli`, `Commands`), `run()` dispatcher, `main()`
- `src/io.rs`          — System boundary (real SSH, stdin, process entry point)
  - Excluded from coverage via `--ignore-filename-regex 'io\.rs$'`
- `src/error.rs`       — `SkulkError` enum, SSH error classification
- `src/ssh.rs`         — `Ssh` trait (injectable for testing)
- `src/config.rs`      — `Config` struct, `.skulk/config.toml` loading (with `.skulk.toml` legacy fallback)
- `src/util.rs`        — Validation, shell escaping, shared helpers
- `src/display.rs`     — Session types, table formatting, GC summary display
- `src/inventory.rs`   — `AgentInventory`, single-roundtrip state gathering
- `src/testutil.rs`    — `MockSsh`, `test_config()`, mock builders (test-only)
- `src/commands/`      — One module per command group, each with co-located tests
  - `init.rs`, `list.rs`, `pull.rs`, `new.rs`, `destroy.rs`, `interact.rs`, `gc.rs`

## Error Handling

- No `.unwrap()` or `.expect()` in production code — use `thiserror` + `anyhow`
- Every `?` gets `.context("meaningful message")`
- Domain-specific error enum `SkulkError` in `error.rs`

## Testing

- TDD: Red-Green-Refactor — write failing test first
- `MockSsh` in tests injects responses; no real SSH in unit tests
- Test names describe behavior: `fn returns_error_when_agent_not_found()`
- Each test: Arrange, Act, Assert — clearly separated

## Commits

- Never mix structural and behavioral changes in the same commit
- Structural: `refactor: description`
- Behavioral: `feat: description` or `fix: description`
- Every commit must leave all tests passing

## Branch Discipline

You are working in a git worktree with its own branch already checked out.
**Always commit on the current branch.** Do NOT create new branches (e.g. `feat/X`).
Run `git branch --show-current` if unsure — that is your branch, use it.

## Configuration

Runtime config from `.skulk/config.toml` (searched upward from cwd; legacy `.skulk.toml` still loaded with a deprecation warning):

```toml
host = "your-server"
session_prefix = "skulk-"
base_path = "~/your-project"
worktree_base = "~/your-project-worktrees"
# default_branch = "main"   # optional, defaults to "main"
```

## Never Do

- `#[allow(dead_code)]` or `#[allow(unused)]` — delete unused code
- `#[allow(clippy::...)]` without a justifying comment
- `.clone()` to satisfy the borrow checker without trying ownership restructure first
- `unsafe` without `// SAFETY:` comment
- `async` unless genuinely doing I/O
- `dbg!()` or `println!()` debugging in committed code

## Preferred Crates

`serde`, `clap`, `thiserror`, `toml`
