---
created: 2026-06-24
---

# Add OutputFormat enum and emit_json helper

Add `OutputFormat` enum (`Human` / `Json`) to `src/config.rs`, add `output_format` field to `Config` struct defaulting to `Human`, update config loading to parse `output_format` from TOML, and add shared `emit_json(value: &impl Serialize)` helper to `src/display.rs`.
