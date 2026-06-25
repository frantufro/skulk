---
created: 2026-06-25
---

# Move skulk update out of the config-required gate

# Move `skulk update` out of the config-required gate

## Context

`skulk update` is a self-updater: it reads the current version from
`env!("CARGO_PKG_VERSION")` at compile time, fetches the latest GitHub release,
downloads the binary for the current platform, and replaces the running binary
via `current_exe()`. It does **not** read any project configuration.

Evidence:
- `src/commands/update.rs:225` — `pub(crate) fn cmd_update(client: &impl HttpClient) -> Result<(), SkulkError>` takes only an HTTP client, no `Config`.
- `src/main.rs:612` — `Commands::Update => update::cmd_update(&update::UreqClient)` passes no `cfg`.

Despite this, running `skulk update` from a directory without `.skulk/config.toml`
fails with:

```
skulk: No .skulk/config.toml found. Run `skulk init` to set up this project.
```

The cause is purely structural. In `src/io.rs` (around line 595-625), `main()`
special-cases `Commands::Init` and `Commands::Completions` to run *before*
config loading, then falls through to a blanket gate:

```rust
// All other commands require config
let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
let mut cfg = match load_config(&cwd) {
    Ok(c) => c,
    Err(e) => {
        eprintln!("skulk: {e}");
        std::process::exit(1);
    }
};
```

`Update` sits in that bucket even though it never uses the loaded `cfg`, so it
pays the config tax for no reason. This forced a workaround during the v0.7.0
release: `skulk update` had to be run from `~/skulk` on the remote rather than
from any directory.

Note: this is specifically about `skulk update` (binary self-update). It is NOT
about `skulk pull` (`Commands::Pull`, "Update the base clone on the remote
server to latest main"), which legitimately requires config because it SSHes to
`host` and runs git in `base_path`. Do not touch `Pull`.

## What to do

Make `skulk update` runnable from any directory, with no `.skulk/config.toml`
required — matching the existing treatment of `Completions`.

In `src/io.rs`, add an early-return branch for `Commands::Update` alongside the
existing `Commands::Completions` branch (before the "All other commands require
config" block), dispatching directly to `update::cmd_update`:

```rust
// Self-update never reads project config — handle it before the config gate.
if matches!(cli.command, Commands::Update) {
    if let Err(e) = update::cmd_update(&update::UreqClient) {
        // Match the existing run() error formatting for the update command.
        eprintln!("skulk update: {e}");
        std::process::exit(1);
    }
    return;
}
```

Then REMOVE the now-dead `Commands::Update => update::cmd_update(&update::UreqClient)`
arm from `run()` in `src/main.rs` (around line 612), since dispatch now happens
in `io.rs` before `run()` is reached. Also remove the `Commands::Update => "update"`
command-name mapping at `src/main.rs:460` ONLY if it becomes unused after the
dispatch arm is removed — verify with `cargo build` (the match in `run()` must
still be exhaustive; if `run()` still receives `Commands::Update` in any path,
keep a handling arm). Prefer: keep `run()` exhaustive by having its `Update` arm
be unreachable-free — the cleanest approach is that `run()` never sees `Update`
because `main()` intercepts it, but the `Commands` enum match in `run()` must
still compile. If clap/match exhaustiveness requires an arm, make it
`Commands::Update => unreachable!("update is handled in main() before run()")`
with a justifying comment, OR (preferred) keep the dispatch in `run()` and
instead skip only the config-loading for `Update`. Choose whichever keeps the
code simplest and clippy-clean; document the choice in a comment.

### Recommended simplest design

Rather than splitting dispatch across `io.rs` and `run()`, prefer the
`Completions` pattern exactly: intercept `Update` in `main()` before config
loading and `return`, and delete the `run()` arm. Confirm `run()` still compiles
(its `Commands` match may need no `Update` arm if `run()` is only ever called
after `main()` has already intercepted `Init`, `Completions`, and `Update` — but
the match must remain exhaustive over the `Commands` enum; if so, add the
`unreachable!` arm with a comment).

## Tests

There is an existing pattern for `run()` dispatch tests in `src/main.rs`
(`run_dispatches_*`). Mirror the approach used for whichever path you keep:

- If `Update` is intercepted in `main()`: `main()` itself is in `io.rs` and is
  excluded from coverage (`io.rs`), so a direct unit test there is not required.
  Instead, add/keep a unit test for `cmd_update` behavior in
  `src/commands/update.rs` using the existing `MockHttpClient` (or equivalent)
  to confirm the command logic is unchanged. Verify no test asserted that
  `skulk update` requires config.
- Add a CLI parse test (mirroring `archive_accepts_reason_flag` style) if one
  does not already exist, confirming `Cli::try_parse_from(["skulk", "update"])`
  parses to `Commands::Update`.

Manually verify the fix end-to-end:

```bash
cargo build --release
cd /tmp && /path/to/target/release/skulk update --help   # must not error about config
```

(`--help` already bypasses dispatch; the real check is running `skulk update`
from a directory with no `.skulk/` and confirming it proceeds to the GitHub
fetch instead of printing the "No .skulk/config.toml found" error.)

## Verification

```
cargo fmt
cargo clippy -- -D warnings -W clippy::pedantic
cargo test
```

All must pass, zero clippy warnings. Confirm `skulk update` run from a directory
with no `.skulk/config.toml` no longer prints the config error.
