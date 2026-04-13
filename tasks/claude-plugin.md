---
status: READY
---

Build a Claude Code plugin (or skill bundle) that teaches Claude how to use skulk.

**Use case**: a user in their main Claude Code session can say "spin up an agent to look into the auth bug" and Claude knows the right skulk commands to run, the conventions (worktrees, branch naming, .skulk/init.sh, etc.), and how to monitor/ship the result.

**Likely shape** (decide at implementation time):
- A Claude Code plugin shipped alongside the skulk binary or installable separately
- Bundle of skills covering: `new` (with `--from`, `--github`, `--remote-control` variants), `list`, `logs`, `connect`, `disconnect`, `send`, `diff`, `push`, `ship`, `archive`, `restart`, `wait`, `gc`, `destroy`
- Each skill: short description, when-to-use, example invocations, common pitfalls
- Reference page covering the overall workflow (init → new → monitor → diff → ship)

**Distribution**:
- Decide whether to publish as a standalone plugin in the marketplace, ship inside this repo as installable assets, or both

**Out of scope**:
- Plugin for the spawned agents themselves — they're already inside a skulk-managed session and shouldn't be calling `skulk new` recursively (could revisit if useful)

**Touches**:
- New top-level directory (e.g., `plugin/` or `claude/`) with the skill files
- README — link to install instructions
