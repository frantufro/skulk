# Skulk

Dead simple agent management over SSH. For humans and agents alike.

Skulk spins up [Claude Code](https://docs.claude.com/en/docs/claude-code) agents on a remote server — each in its own tmux session and git worktree, fully isolated, zero conflicts.

```
$ skulk new auth-refactor "Refactor the auth middleware to use JWT"
Created agent 'auth-refactor' on dev-server.
Prompt delivered to skulk-auth-refactor.

$ skulk new fix-pagination "Fix the off-by-one error in /api/users pagination"
Created agent 'fix-pagination' on dev-server.
Prompt delivered to skulk-fix-pagination.

$ skulk list
NAME                 STATUS     UPTIME        WORKTREE
auth-refactor        detached   3m            ~/myproject-worktrees/skulk-auth-refactor
fix-pagination       idle       1m            ~/myproject-worktrees/skulk-fix-pagination
```

Two agents. Two branches. Two worktrees. Running simultaneously on one machine.

## Why

Claude Code is great, but it works on one thing at a time. If you have a beefy dev server sitting around, Skulk lets you fan out: spin up five agents on five different tasks and check back when they're done. Each agent works in its own git worktree, so there are no merge conflicts mid-work.

Skulk is built for humans and AI agents. Use it as a regular CLI, wire it into scripts, or let an orchestrator agent drive it via the [Claude Code plugin](#claude-code-plugin). One Claude session can spin up, monitor, and ship work from a fleet of remote agents running in parallel.

## Requirements

**Local machine:** macOS or Linux with an OpenSSH client.

**Remote server:** Debian-based Linux (Ubuntu, Debian, etc.) with SSH access and key-based auth. Skulk's `init` command installs everything else (tmux, git, Claude Code). Other distros may work but are not officially supported yet.

**Localhost mode:** Set `host = "localhost"` in `.skulk/config.toml` to run agents on the same machine without SSH.

## Install

```bash
curl -sSL https://raw.githubusercontent.com/frantufro/skulk/main/install.sh | sh
```

Or via Homebrew (macOS and Linux):

```bash
brew install frantufro/tap/skulk
```

Or build and install from source:

```bash
git clone https://github.com/frantufro/skulk.git
cd skulk
cargo install --path .
```

## Quick Start

### 1. Initialize

Run `skulk init` in your project directory. The interactive wizard will:

- Detect your git remote and default branch
- Ask for your SSH host and test connectivity
- Generate a `.skulk/config.toml` file
- Optionally set up the remote server (install tools, clone repo, create worktree directory)

```bash
skulk init
```

The generated config looks like:

```toml
host = "your-server"
session_prefix = "my-project-"
base_path = "~/my-project"
worktree_base = "~/my-project-worktrees"
default_branch = "main"
```

| Field | Description |
|-------|-------------|
| `host` | SSH host (must be reachable via `ssh your-server`), or `localhost` / `127.0.0.1` / `::1` to run commands on the local machine without SSH |
| `session_prefix` | Prefix for tmux sessions and git branches |
| `base_path` | Path to the main git clone on the remote |
| `worktree_base` | Directory where agent worktrees are created |
| `default_branch` | Branch that new worktrees are based on (default: `main`) |

The config file is searched upward from your current directory, so you can place it at your project root.

### 2. Spin up agents

```bash
# Create an agent and give it a task
skulk new fix-bug "Fix the null pointer exception in UserService.java"

# Create an agent without a prompt (starts Claude Code, you connect and interact manually)
skulk new explore

# Load the prompt from a local task file
skulk new big-feature --from ./tasks/refactor.md

# Load the prompt from a GitHub issue on the current repo
skulk new fix-123 --github 123

# Spin up an agent on a specific model
skulk new big-refactor --model opus "Untangle the auth middleware"

# Create an agent reachable from the Claude Code mobile/web app
skulk new mobile-task --remote-control "Fix the login bug"

# Pass arbitrary extra flags through to Claude Code.
# Note the inner single quotes around Bash(...): --claude-args is typed into
# the remote shell by tmux, so shell metacharacters (parens, globs, $, ;, …)
# must be pre-quoted to reach Claude literally.
skulk new scoped --claude-args "--allowed-tools 'Bash(gh pr:*)'" "Triage open PRs"
```

By default Skulk launches Claude Code **without** `--remote-control`. Skulk's own commands (`connect`, `logs`, `send`) talk to the agent through tmux directly, and leaving `--remote-control` on triggers an upstream idle-death bug ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)) that kills agents after ~20 minutes of inactivity. Only opt in when you want to drive an agent from the Claude Code mobile/web app, and don't use it for long autonomous tasks.

### 3. Monitor and interact

```bash
# See what's running
skulk list

# View an agent's terminal output
skulk logs fix-bug

# Follow output in real time (like tail -f)
skulk logs fix-bug --follow

# View scrollback history
skulk logs fix-bug --lines 500

# Attach to an agent's live tmux session (interactive)
skulk connect fix-bug
# Detach with Ctrl+B then D

# Send a follow-up prompt to a running agent
skulk send fix-bug "Actually, also add a test for the edge case"
```

### 4. Pull changes and clean up

```bash
# Update the base clone on the remote
skulk pull

# Destroy a specific agent (session + worktree + branch)
skulk destroy fix-bug

# Destroy all agents at once
skulk destroy-all

# Clean up orphaned resources (sessions without worktrees, etc.)
skulk gc

# Preview what gc would clean without actually doing it
skulk gc --dry-run
```

## Commands

| Command | Description |
|---------|-------------|
| `skulk init` | Interactive setup wizard — generates config and optionally provisions the remote server |
| `skulk new <name>` | Create a new agent with its own worktree (`--from`, `--github`, `--model`, `--claude-args`) |
| `skulk list` | List all agents with status, idle state, uptime, and worktree path |
| `skulk status <name>` | Detailed single-agent view: status, commits ahead, files changed, uptime |
| `skulk connect <name>` | Attach to an agent's live tmux session |
| `skulk disconnect <name>` | Detach all clients from an agent's session |
| `skulk logs <name>` | View an agent's terminal output (`-f` to follow, `-l` for scrollback) |
| `skulk send <name> <prompt>` | Send a prompt to a running agent (`--from` to read from a file) |
| `skulk diff <name>` | Show git diff against the default branch (`--stat`, `--name-only`) |
| `skulk git-log <name>` | Show commits on the agent's branch not in the default branch |
| `skulk transcript <name>` | Dump full tmux scrollback (`--output` to write to a file) |
| `skulk push <name>` | Push the agent's branch to origin |
| `skulk ship <name>` | Push and open a PR with a Claude-authored description |
| `skulk wait <name>` | Block until the agent is idle (`--all` for all agents, `--timeout <secs>` to cap the wait) |
| `skulk archive <name>` | Kill tmux session but keep worktree and branch intact |
| `skulk restart <name>` | Restart an archived or crashed agent in its existing worktree |
| `skulk doctor` | Health check — verify SSH, tools, base clone, and worktree directory |
| `skulk pull` | Update the base clone (`git pull --ff-only`) |
| `skulk destroy <name>` | Destroy an agent (session, worktree, and branch) |
| `skulk destroy-all` | Destroy all agents at once |
| `skulk gc` | Clean up orphaned sessions, worktrees, and branches |

## Per-Agent Setup (Init Hook)

Skulk runs an optional setup script inside each agent's tmux session before Claude starts — useful for `docker compose up`, migrations, dependency installs, mock services, etc.

**Convention:** put the script at `.skulk/init.sh` in your repo. Override the path with `init_script = "scripts/setup-agent.sh"` in `.skulk/config.toml` if you prefer.

**Project env file:** `.skulk/.env` lives locally (gitignored — `skulk init` adds the entry automatically) and almost always contains secrets. On `skulk new`, Skulk copies it to the agent's worktree at `<worktree>/.env` so dotenv-aware project tooling picks it up, and Skulk also `source`s it before running `init.sh` so the script sees the same vars (e.g. `$DATABASE_URL` for migrations).

> ⚠️ **Security:** shipping `.skulk/.env` sends your local secrets to the remote server. Review what's in it before running `skulk new`, especially on shared hosts.

**Env vars passed to `init.sh`:**

| Variable | Example |
|----------|---------|
| `SKULK_AGENT_NAME` | `auth-refactor` |
| `SKULK_SESSION` | `myproject-auth-refactor` |
| `SKULK_BRANCH` | `myproject-auth-refactor` |
| `SKULK_WORKTREE` | absolute path to the worktree |

**Failure handling — hard fail:** if `init.sh` exits non-zero, Claude does not start. The tmux session stays open with the error visible — run `skulk connect <name>` to investigate. For per-step opt-outs, use the usual shell idiom: `risky_command || true`.

`skulk init` writes `.skulk/init.sh.example` — rename it to `.skulk/init.sh` and customize to enable.

## Claude Code Plugin

Skulk ships a Claude Code plugin that teaches Claude how to drive skulk
directly. Once installed, you can ask Claude things like "spin up three
agents on these tasks" or "check on the running agents" and it'll run
the right skulk commands for you.

Install via the plugin marketplace from inside Claude Code:

```
/plugin marketplace add frantufro/claude-plugins
/plugin install skulk@frantufro-plugins
```

The plugin contributes a `skulk-agent-management` skill that covers the
full agent lifecycle (create, monitor, interact, review, ship, clean up).

## How It Works

```
Local                          Remote Server
─────                          ─────────────
skulk init ──────SSH──►  Tests connectivity
                         Installs tmux, git, claude (if missing)
                         Clones repo to base_path
                         Creates worktree_base directory

skulk new auth ──SSH──►  git worktree add ~/worktrees/skulk-auth
                         tmux new-session -d -s skulk-auth
                         (starts claude in the worktree)

skulk send auth ──SSH──► tmux send-keys "your prompt" Enter
                         (verifies delivery via pane content diff)

skulk connect auth ──SSH──► tmux attach -t skulk-auth
                            (interactive terminal, Ctrl+B D to detach)

skulk destroy auth ──SSH──► tmux kill-session -t skulk-auth
                            git worktree remove skulk-auth
                            git branch -D skulk-auth
```

Each agent is a tmux session running Claude Code inside its own git worktree. Worktrees share the same `.git` directory as the base clone but have independent working trees and branches — so agents can edit files simultaneously without stepping on each other.

## Agent Names

Names must be lowercase letters, digits, and hyphens. 1-30 characters. No leading, trailing, or consecutive hyphens.

```
skulk new my-feature      # valid
skulk new fix-123         # valid
skulk new My_Feature      # invalid (uppercase, underscores)
skulk new -bad-name-      # invalid (leading/trailing hyphens)
```

## Flags

| Flag | Scope | Description |
|------|-------|-------------|
| `--no-color` | Global | Disable colored output (also respects `NO_COLOR` env var) |
| `--from <FILE>` | `new`, `send` | Load the prompt from a local text file instead of the positional argument |
| `--github <ISSUE_ID>` | `new` | Load the prompt from a GitHub issue (title, body, comments) via `gh` on the remote. Mutually exclusive with `--from`. |
| `--remote-control` | `new` | Launch Claude with `--remote-control` so the agent is accessible from the Claude Code mobile/web app. Off by default because of an upstream idle-death bug ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)) that kills agents after ~20 min of inactivity — see Quick Start |
| `--model <NAME>` | `new` | Pass `--model <name>` through to Claude Code (e.g. `opus`, `sonnet`, `claude-opus-4-7`). Restricted to `[A-Za-z0-9._-]`. |
| `--claude-args <ARGS>` | `new` | Extra flags appended to the Claude Code launch command. Typed into the remote shell by tmux, so shell metacharacters are re-evaluated — pre-quote for the inner shell (e.g. `--claude-args "--allowed-tools 'Bash(gh pr:*)'"`). |
| `--stat` | `diff` | Summary of changed files (insertions/deletions), mutually exclusive with `--name-only` |
| `--name-only` | `diff` | Paths of changed files only |
| `--follow`, `-f` | `logs` | Stream output in real time |
| `--lines`, `-l` | `logs` | Number of scrollback lines to show |
| `--output <FILE>`, `-o` | `transcript` | Write transcript to this file instead of stdout |
| `--all` | `wait` | Wait for every running agent instead of one |
| `--timeout <SECS>` | `wait` | Maximum seconds to wait before giving up (default: 1800; applies per agent with `--all`) |
| `--force` | `pull` | Hard-reset to `origin/main` instead of fast-forward |
| `--force` | `destroy`, `destroy-all` | Skip the confirmation prompt |
| `--dry-run` | `gc` | Preview what would be cleaned |

## Error Handling

Skulk gives you actionable diagnostics instead of raw SSH errors:

- **Connection refused** — check that SSH is running on the remote
- **Host key verification failed** — accept the host key first
- **Permission denied** — check your SSH key or config
- **Agent not found** — the named agent doesn't exist; use `skulk list` to see what's running
- **Base clone missing** — run `skulk init` to set up the remote server

Destructive operations (`destroy`, `destroy-all`) require confirmation unless `--force` is passed. If agent creation fails partway through (e.g., tmux session can't start), the worktree is automatically rolled back.

## Contributing

Contributions welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
ground rules, project layout, and release workflow.

## License

[MIT](LICENSE)
