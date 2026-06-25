---
created: 2026-06-24
---

# JSON output for list command

Emit a bare JSON array of agent objects when `cfg.output_format == OutputFormat::Json`. Each object: `name`, `status`, `branch`, `uptime_secs`, `last_activity`.
