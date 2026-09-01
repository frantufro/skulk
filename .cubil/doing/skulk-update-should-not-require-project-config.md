---
created: 2026-05-15
---

# `skulk update` should not require a `.skulk/config.toml`

Running `skulk update` from a directory without `.skulk/config.toml` errors out:

```
Warning: skulk v0.5.1 is available (you have v0.5.0). Run `skulk update` to upgrade.
skulk: No .skulk/config.toml found. Run `skulk init` to set up this project.
```

`skulk update` is a self-update operation — it has no project-level inputs and produces no project-level outputs. It should work from any directory, including the user's home directory or wherever the skulk binary happens to be launched from.

The startup version-check warning correctly fires *before* the config-loading step bails, which is the inversion that exposes the bug — the warning instructs the user to run `skulk update`, but running it from the same place fails.

## Fix

Move the config-loading step into `run()` per-command, rather than unconditionally at the top of `main`/`run`. Commands that don't need config (`update`, `completions`, possibly `init`) should run without it.

## Touches

- `src/main.rs` — restructure `run()` so config loading is per-command.
- `src/commands/update.rs` — confirm it doesn't read config.
- Tests for `cmd_update` running without a config file.
