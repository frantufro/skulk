---
created: 2026-05-05
---

# Implement skulk update command

Add a `skulk update` subcommand that self-updates the binary, and warn on every run if the installed version is behind the latest release.

## Scope

- `skulk update`: fetch the latest release from GitHub (frantufro/skulk), download the binary for the current platform, replace the running binary in-place, and print the new version.
- Version staleness check: on every command run, compare the installed version against the latest GitHub release tag (cache the check locally to avoid hitting the network on every invocation — e.g. once per day). If behind, print a warning to stderr: `Warning: skulk vX.Y.Z is available (you have vA.B.C). Run `skulk update` to upgrade.`

## Implementation notes

- Use `env!("CARGO_PKG_VERSION")` for the current version.
- Hit the GitHub Releases API (`https://api.github.com/repos/frantufro/skulk/releases/latest`) to get the latest tag.
- Cache the latest-version response in a file under the user's cache dir (e.g. `~/.cache/skulk/latest-version`) with a timestamp; re-fetch only if older than 24 h.
- Replace the binary atomically (write to a temp file, then `rename`).
- Gate the staleness check with `SKULK_NO_UPDATE_CHECK=1` env var to opt out.
- No `async` — use a blocking HTTP client (e.g. `ureq`).

## Acceptance criteria

- `skulk update` downloads and replaces the binary, prints confirmation.
- Running any skulk command when behind prints the warning to stderr and exits normally.
- `SKULK_NO_UPDATE_CHECK=1` silences the staleness check.
- All existing tests still pass; new behaviour is unit-tested with mocked HTTP responses.
