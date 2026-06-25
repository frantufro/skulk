# output-format

Add programmatic JSON output mode to skulk. Design decisions: global --json/--human flags (mutually exclusive), output_format field in config.toml (default: human), CLI flag overrides config, emit_json helper for all structured output, structured JSON errors on stderr.

## Milestone: Foundation
- [ ] add-outputformat-enum-and-emit-json-helper — Add OutputFormat enum and emit_json helper
- [ ] add-json-human-global-cli-flags — Add --json / --human global CLI flags

## Milestone: Command output
- [ ] json-output-for-list-command — JSON output for list command
- [ ] json-output-for-status-command — JSON output for status command
- [ ] json-output-for-gc-command — JSON output for gc command
- [ ] json-output-for-logs-command — JSON output for logs command
- [ ] json-output-for-diff-command — JSON output for diff command
- [ ] json-output-for-wait-command — JSON output for wait command
- [ ] json-output-for-ship-command — JSON output for ship command

## Milestone: Polish
- [ ] structured-json-errors-on-stderr — Structured JSON errors on stderr
- [ ] document-output-format-in-config-and-claude-md — Document output_format in config and CLAUDE.md
- [ ] tests-for-json-output-mode — Tests for JSON output mode
