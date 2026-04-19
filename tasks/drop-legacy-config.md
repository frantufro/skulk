---
status: READY
---

Remove the `.skulk.toml` legacy config fallback.

The migration from `.skulk.toml` to `.skulk/config.toml` landed in `f78a744`. The fallback has been shipping since then with a deprecation warning. Cut it at 0.2.0 — skulk is pre-1.0, the migration window is sufficient.

**Changes**:
- `src/config.rs` — remove the fallback path and deprecation warning; only look for `.skulk/config.toml`
- Remove any tests exercising the legacy fallback
- README — no `.skulk.toml` references remain (verify)
