---
status: READY
---

Publish a Homebrew formula for skulk.

Currently installable via `curl | sh` and source builds. A `brew install skulk` (or `brew tap`) reaches macOS users more naturally.

**Options** (decide at implementation time):
- Homebrew tap (`frantufro/tap`) — lower friction to publish, full control
- Core Homebrew — higher visibility, stricter review process, requires notable adoption

**Formula**:
- Download the pre-built binary from GitHub Releases (Apple Silicon)
- Or build from source via `cargo install` if Homebrew prefers that pattern

**Touches**:
- New repo or directory for the tap (e.g. `homebrew-tap/Formula/skulk.rb`)
- CI — automate formula bump on new GitHub Release
- README — add `brew install` instructions
