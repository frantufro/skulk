---
name: skulk-agent-management
description: >
  Manage remote Claude Code agents with skulk. Use this skill when the user
  wants to spin up coding agents on a remote server, monitor their progress,
  review their work, or ship their changes. Covers the full agent lifecycle:
  create, monitor, interact, review, ship, and clean up. TRIGGER when: user
  mentions skulk, remote agents, spinning up agents, parallel agents, or
  managing a fleet of coding agents.
allowed-tools: [Bash, Read, Glob, Grep]
---

# Skulk Agent Management

Skulk manages remote Claude Code agents over SSH. Each agent runs in its own
tmux session and git worktree on a remote server, fully isolated. You can run
many agents in parallel on different tasks.

## Prerequisites

The project must have a `.skulk/config.toml` file. If it doesn't exist, run
`skulk init` to create one interactively. Run `skulk doctor` to verify the
environment is healthy.

## Agent Lifecycle

### 1. Create Agents

```bash
# Basic — agent starts Claude Code, ready for manual interaction
skulk new <name>

# With a prompt from a local task file
skulk new <name> --from <file>

# With a prompt from a GitHub issue
skulk new <name> --github <issue-id>

# With a specific model
skulk new <name> --model opus --from <file>

# With extra Claude flags
skulk new <name> --claude-args "--allowed-tools 'Bash(gh pr:*)'"

# With mobile/web app access (has idle-death limitation)
skulk new <name> --remote-control
```

Agent names must be lowercase letters, digits, and hyphens. 1-30 characters.
No leading, trailing, or consecutive hyphens.

When creating multiple agents, launch them in parallel:
```bash
skulk new auth-fix --from tasks/auth-fix.md
skulk new pagination --from tasks/pagination.md
skulk new refactor --from tasks/refactor.md
```

### 2. Monitor Agents

```bash
# Fleet overview — all agents with status, idle state, uptime
skulk list

# Detailed single-agent view — status, commits, files changed
skulk status <name>

# View recent terminal output
skulk logs <name>

# Follow output in real time
skulk logs <name> --follow

# View scrollback history
skulk logs <name> --lines 500

# Block until an agent finishes its current turn
skulk wait <name>

# Block until ALL agents are idle
skulk wait --all
```

The IDLE column in `skulk list` shows: `working` (actively coding),
`idle` (finished, ready for review), or `stopped` (session ended).

### 3. Interact with Agents

```bash
# Send a follow-up prompt
skulk send <name> "Also add tests for the edge case"

# Send a prompt from a file
skulk send <name> --from <file>

# Attach to the live tmux session (interactive terminal)
skulk connect <name>
# Detach with Ctrl+B then D

# Detach all other clients from a session
skulk disconnect <name>
```

### 4. Review Agent Work

```bash
# See what changed (full diff against default branch)
skulk diff <name>

# Summary of files changed
skulk diff <name> --stat

# Just the file paths
skulk diff <name> --name-only

# Commit history on the agent's branch
skulk git-log <name>

# Dump the full session transcript
skulk transcript <name>

# Save transcript to a file
skulk transcript <name> --output review.txt
```

### 5. Ship Agent Work

```bash
# Push the agent's branch to origin
skulk push <name>

# Push AND open a PR with a Claude-authored description
skulk ship <name>
```

`skulk ship` requires `gh` and `claude` on the remote. It generates the PR
description by running `claude -p` against the diff.

### 6. Pause and Resume Agents

```bash
# Stop the agent but keep its worktree and branch intact
skulk archive <name>

# Restart an archived or crashed agent in its existing worktree
skulk restart <name>

# Restart with a different model or extra flags
skulk restart <name> --model opus
skulk restart <name> --claude-args "--allowed-tools Bash"
skulk restart <name> --remote-control
```

`restart` launches a fresh Claude session with empty context in the existing
worktree. Use `skulk send` or `claude --continue` inside the session to
resume prior work.

### 7. Replay an Agent's Task

```bash
# Re-run the original prompt on a fresh worktree (auto-names task-2, task-3, …)
skulk replay <name>

# Explicit new name
skulk replay <name> --as retry-opus

# Override the model for the replay
skulk replay <name> --model opus

# Combine: new name + different model
skulk replay <name> --as retry-sonnet --model sonnet
```

`skulk replay` reads the prompt that was passed to `skulk new` (stored at
`~/.skulk/prompts/<session>.txt` on the remote) and creates a fresh agent
with the same task. Useful for benchmarking, retrying with a different model,
or getting a second opinion.

### 8. Clean Up

```bash
# Destroy a specific agent (session + worktree + branch)
skulk destroy <name>

# Destroy all agents at once
skulk destroy-all

# Clean up orphaned resources (sessions without worktrees, etc.)
skulk gc

# Preview what gc would clean
skulk gc --dry-run

# Update the base clone on the remote
skulk pull
```

`destroy` and `destroy-all` require confirmation unless `--force` is passed.

## Environment Health

```bash
# Verify SSH, tmux, claude, gh, base clone, worktree directory
skulk doctor

# Set up a new project for skulk
skulk init

# Generate shell tab-completion script
skulk completions bash   # or zsh, fish
```

## Init Hook

Skulk runs `.skulk/init.sh` (if present) inside each agent's tmux session
before Claude starts. Use it for `docker compose up`, migrations, dependency
installs, etc.

The script receives these environment variables:
- `SKULK_AGENT_NAME` — e.g. `auth-refactor`
- `SKULK_SESSION` — full tmux session name
- `SKULK_BRANCH` — git branch name
- `SKULK_WORKTREE` — absolute path to the worktree

If `.skulk/.env` exists locally, skulk copies it to the agent's worktree and
sources it before running `init.sh`.

If `init.sh` exits non-zero, Claude does not start. The session stays open
for investigation via `skulk connect <name>`.

## Common Workflows

### Fan out on multiple tasks
```bash
skulk new task-a --from tasks/a.md
skulk new task-b --from tasks/b.md
skulk new task-c --from tasks/c.md
skulk wait --all
skulk list
# Review each with: skulk diff <name>, skulk git-log <name>
# Ship each with: skulk ship <name>
```

### Retry a task with a different model
```bash
# Replay the original prompt on a fresh agent with a different model
skulk replay slow-agent --as fast-agent --model sonnet

# Or archive and start fresh manually
skulk archive slow-agent
skulk new fast-agent --model sonnet --from tasks/same-task.md
```

### Check on agents and relay answers
When asked to check agent status, run `skulk list` to see all agents, then
`skulk logs <name> --lines 5` for each to understand what they're doing. If
an agent is idle and waiting for input, relay the user's answer with
`skulk send <name> "the answer"`.

### Review and merge
```bash
skulk diff <name> --stat       # quick summary
skulk diff <name>              # full diff
skulk git-log <name>           # commit history
skulk ship <name>              # push + open PR
skulk archive <name>           # stop the agent
```

## Important Notes

- Agents launched with `--remote-control` die after ~20 min of inactivity
  due to an upstream bug. Only use it for interactive mobile/web sessions.
- `skulk send` delivers text via tmux — shell metacharacters in prompts are
  safe but long prompts may be truncated. Use `--from` for large prompts.
- Each agent works in its own git worktree. There are no merge conflicts
  between agents during work. Conflicts only arise at merge time.
- `skulk ship` generates PR descriptions using `claude -p` on the remote.
  Both `gh` and `claude` must be installed and authenticated there.
