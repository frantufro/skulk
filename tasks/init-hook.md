---
status: DONE
implemented: b604220
---

Add a pre-launch hook script that runs inside the agent's tmux session before Claude Code starts. Use case: setting up the project environment per agent (docker compose up, migrations, dependency installs, mock services, env sourcing, etc.).

**Discovery** (convention + config override):
- Convention: `.skulk/init.sh` in the project root (next to `.skulk/config.toml`)
- Override: `init_script = "scripts/setup-agent.sh"` in `config.toml`

**Project env file**:
- `.skulk/.env` lives locally (gitignored — almost always contains secrets); `skulk init` adds the entry to `.gitignore` automatically
- On `skulk new`, skulk copies `.skulk/.env` to the agent's worktree at `<worktree>/.env` so dotenv-aware project tooling auto-discovers it
- skulk also `source`s `.env` before running `init.sh`, so init.sh has access to the same vars (e.g. `$DATABASE_URL` for migrations)

**When it runs**:
- Inside the tmux session, as the first command, before claude is launched
- Output visible in `skulk logs <name>` from the start

**Failure handling — hard fail**:
- If `init.sh` exits non-zero, claude does not start
- Tmux session stays open with the error visible — `skulk connect <name>` to investigate
- Per-step opt-out is the script's responsibility: `risky_command || true`
- No `init_strict` config knob; add only if a real use case emerges

**Environment variables passed to `init.sh`**:
- `SKULK_AGENT_NAME` — e.g. `auth-refactor`
- `SKULK_SESSION` — full tmux session name, e.g. `myproject-auth-refactor`
- `SKULK_BRANCH` — git branch (same as `SKULK_SESSION`)
- `SKULK_WORKTREE` — absolute path to the worktree the script runs in

**Working directory**: the agent's worktree (`SKULK_WORKTREE`).

**Touches**:
- `src/commands/new.rs` — agent launch sequence: copy `.env` → source `.env` → run `init.sh` → start claude
- `src/config.rs` — read optional `init_script` field
- `src/commands/init.rs` — wizard: create `.skulk/init.sh.example`, gitignore `.skulk/.env`
- README — document the hook, env vars, `.env` copy behavior, `|| true` pattern, and security implication (env shipped from local to remote)

**Depends on**: `skulk-directory` (the `.skulk/` directory must exist).
