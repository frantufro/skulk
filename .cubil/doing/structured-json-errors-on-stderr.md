---
created: 2026-06-24
---

# Structured JSON errors on stderr

In JSON mode, write `{"error": "<human message>", "code": "<snake_case_SkulkError_variant>"}` to stderr instead of plain text. Derive the `code` from the `SkulkError` variant name via `serde` or manual mapping.
