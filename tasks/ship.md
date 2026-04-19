---
status: DONE
implemented: 3555d2d
---

Add `skulk ship <name>` — push the agent's branch and open a PR with a **Claude-authored PR description** (not manual).

**Open design — how to author the description**:
- Send a prompt to the running agent asking it to generate a PR description, then capture pane output
- Spawn a fresh `claude -p` headless call against `git diff <base>...<branch>`
- Pick at implementation time

Requires `gh` CLI on the remote; detect and error cleanly if missing.
