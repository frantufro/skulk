---
status: READY
---

Define and implement the task file format parser and writer.

**Format**: `tasks/<name>.md`. Filename → agent name. YAML frontmatter. Body → agent prompt.

```markdown
---
status: READY
pr: 123          # optional, set by ship
---
<prompt body>
```

Valid statuses: `PLANNING`, `READY`, `PROGRESSING`, `REVIEW`, `DONE`.

**Deliverables**:
- Parser: frontmatter + body extraction, status validation, filename → name validation (reuse existing agent-name rules)
- Writer: update status/fields in place without disturbing other frontmatter
- README documentation

Foundation for `sweep`, `ship-task-integration`, and `task-update`.
