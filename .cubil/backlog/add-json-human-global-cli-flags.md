---
created: 2026-06-24
---

# Add --json / --human global CLI flags

Add mutually exclusive global `--json` and `--human` flags to the root `Cli` struct in `src/main.rs` (using clap `conflicts_with`). Resolve precedence — CLI flag overrides `output_format` in config — and populate `cfg.output_format` before `run()` is called.
