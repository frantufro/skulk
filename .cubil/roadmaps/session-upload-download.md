# session-upload-download

Transfer Claude Code sessions and git branches between local machines and remote skulk agents. Enables bidirectional handoff: `skulk upload` pushes local work to a remote agent; `skulk download` brings a remote agent's work back locally. Depends on: relaxed name validation (allow `feat/foo` style names), `--reason` flag on `skulk archive` (used by download to annotate auto-archive), Claude Code project path encoding helper (for JSONL file transfer), and new `Ssh::download_file` method.

## Milestone: Infrastructure
Tasks in this milestone are INDEPENDENT and can be executed in parallel by separate agents.
- [ ] relax-agent-name-validation-to-allow-uppercase-underscores-and-slashes — Relax agent name validation to allow uppercase, underscores, and slashes
- [ ] add-optional-reason-flag-to-skulk-archive — Add optional --reason flag to skulk archive
- [ ] add-claude-code-project-path-encoding-helper — Add Claude Code project path encoding helper

## Milestone: Commands
Tasks in this milestone are INDEPENDENT of each other but each depends on ALL three Infrastructure tasks above. Execute in parallel after Infrastructure is complete.
- [ ] add-skulk-upload-command-transfer-local-branch-and-claude-session-to-a-remote-agent — Add skulk upload command — transfer local branch and Claude session to a remote agent
- [ ] add-skulk-download-command-bring-a-remote-agent-s-branch-and-claude-session-to-a-local-worktree — Add skulk download command — bring a remote agent's branch and Claude session to a local worktree
