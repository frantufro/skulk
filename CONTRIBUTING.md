# Contributing

Thanks for your interest in improving Skulk. The project is small enough
that a drive-by fix is welcome; the bar is green tests and clean lint.

## Ground rules

Before opening a PR, make sure the following three commands succeed with
zero output on stderr:

```bash
cargo fmt                                        # format
cargo clippy -- -D warnings -W clippy::pedantic  # lint, zero warnings
cargo test                                       # run the full suite
```

A few stylistic conventions that are enforced informally:

- No `.unwrap()` / `.expect()` in production code. Use `thiserror` +
  `anyhow::Context` and attach a message at every `?`.
- TDD. Write the failing test first, then the code. Tests live next to
  the module they exercise (`#[cfg(test)] mod tests`).
- Keep structural and behavioral changes in separate commits.
  `refactor:` vs. `feat:` / `fix:` in the commit subject.
- Don't add `#[allow(…)]` attributes without a justifying comment.
- Default to writing no comments. Only add one when the *why* is
  non-obvious.

## Project layout

```
src/
├── main.rs            CLI definition (clap) and command dispatch
├── io.rs              System boundary: real SSH, stdin, process entry — excluded from coverage
├── error.rs           SkulkError enum and SSH error classification
├── ssh.rs             Ssh trait (injectable for testing)
├── config.rs          Config struct and .skulk/config.toml loading
├── agent_ref.rs       AgentRef — canonical name/branch/worktree/session resolver
├── inventory.rs       AgentInventory — single-roundtrip remote state gathering
├── display.rs         Session types, table formatting, GC summary, colors
├── util.rs            Validation, shell escaping, shared helpers
├── timings.rs         Injectable clock and sleep for deterministic tests
├── testutil.rs        MockSsh and test builders (test-only)
└── commands/          One module per command group, co-located tests
    ├── init.rs        Interactive setup wizard and remote provisioning
    ├── list.rs        Agent listing
    ├── status.rs      Detailed single-agent view
    ├── new.rs         Agent creation with worktree isolation
    ├── destroy.rs     Agent teardown (single and bulk)
    ├── interact.rs    connect, logs, send, disconnect, diff, git-log,
    │                  transcript, push, archive
    ├── restart.rs     Restart a stopped agent in its existing worktree
    ├── ship.rs        Push + Claude-authored PR description + gh pr create
    ├── wait.rs        Block until an agent is idle
    ├── doctor.rs      Runtime health check
    ├── pull.rs        Base clone updates
    ├── gc.rs          Orphan detection and cleanup
    └── prompt_source.rs  Resolve prompt text from --from / --github
```

Everything is tested through the injectable `Ssh` trait with a `MockSsh`
test double — no real SSH calls in the test suite. System-boundary code
lives in `io.rs` and is excluded from coverage (`--ignore-filename-regex
'io\.rs$'`).

## Running Skulk against a real host

The test suite doesn't talk to a real server, but you'll want one for
end-to-end sanity checks. A localhost `.skulk/config.toml` works:

```toml
host = "localhost"
session_prefix = "dev-"
base_path = "~/scratch/skulk-dev"
worktree_base = "~/scratch/skulk-dev-worktrees"
default_branch = "main"
```

Clone a throwaway repo to `base_path`, create `worktree_base`, and run
`cargo run -- init` / `cargo run -- new foo "say hi"` from anywhere
under the project directory.

## Releases

1. `cargo fmt && cargo clippy -- -D warnings -W clippy::pedantic && cargo test`
2. Bump `version` in `Cargo.toml`.
3. Commit as `release: bump to X.Y.Z`.
4. Tag `vX.Y.Z` and push both the commit and the tag.

The `release.yml` GitHub workflow builds binaries for macOS (aarch64),
Linux x86_64, and Linux aarch64, publishes the GitHub release, and
updates the Homebrew tap formula automatically.
