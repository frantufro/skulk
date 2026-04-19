---
status: READY
depends_on: drop-legacy-config
---

Cut a 0.2.0 release.

The feature set has grown massively since 0.1.4: `ship`, `wait`, `restart`, `archive`, `--github`, `--from`, `--model`, `--claude-args`, init hooks, idle detection, and a full refactoring pass. This deserves a version bump.

**Checklist**:
- Bump version in `Cargo.toml`
- Update README commands table (it's missing `diff`, `push`, `archive`, `restart`, `git-log`, `transcript`, `wait`)
- Verify `cargo clippy`, `cargo test`, `cargo fmt` all clean
- Tag and push
- Verify GitHub Release workflow produces the binary

**Depends on**: `drop-legacy-config` (clean break at the version boundary).
