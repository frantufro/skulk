---
status: READY
---

Restructure project config into a `.skulk/` directory.

**Move:**
- `.skulk.toml` → `.skulk/config.toml`

**Future-proofs for:**
- `.skulk/init.sh` (init hook — see `init-hook` task)
- `.skulk/.env` (project env vars — see `init-hook` task)
- `.skulk/hooks/` (future per-event hooks)
- `.skulk/templates/` (future agent templates)

**Migration**:
- `skulk init` writes the new layout
- For at least one release, also accept legacy `.skulk.toml` if `.skulk/config.toml` is absent (fall through with a warning suggesting migration)
- Decide hard-cut vs. dual-support window at implementation time; skulk is at 0.1.x so a hard cut at 0.2.0 is also acceptable

**Touches**:
- `src/config.rs` — config discovery walks up looking for `.skulk/config.toml` (with `.skulk.toml` fallback)
- `src/commands/init.rs` — wizard creates `.skulk/` and writes the new file
- README — update all `.skulk.toml` references

**Foundation for**: `init-hook` and any future `.skulk/`-dwelling features.
