# Skulk

Run multiple Claude Code agents in parallel on a remote server — or on your local machine. Each agent gets its own tmux session and git worktree — fully isolated, zero conflicts.

```
$ skulk new auth-refactor "Refactor the auth middleware to use JWT"
Created agent 'auth-refactor' on dev-server.
Prompt delivered to skulk-auth-refactor.

$ skulk new fix-pagination "Fix the off-by-one error in /api/users pagination"
Created agent 'fix-pagination' on dev-server.
Prompt delivered to skulk-fix-pagination.

$ skulk list
NAME                 STATUS     UPTIME       WORKTREE
auth-refactor        running    3m           ~/myproject-worktrees/skulk-auth-refactor
fix-pagination       running    1m           ~/myproject-worktrees/skulk-fix-pagination
```

Two agents. Two branches. Two worktrees. Running simultaneously on one machine.

## Why

Claude Code is great, but it works on one thing at a time. If you have a beefy dev server sitting around, Skulk lets you fan out: spin up five agents on five different tasks and check back when they're done. Each agent works in its own git worktree, so there are no merge conflicts mid-work.

## Requirements

**Local machine:** OpenSSH client (not needed if running in localhost mode), Rust toolchain (to build Skulk)

**Remote server:** SSH access with key-based auth. Skulk's `init` command will install everything else (tmux, git, Claude Code).

**Localhost mode:** Set `host = "localhost"` in `.skulk/config.toml` to run commands directly via `sh -c` and skip SSH entirely. Useful when running Skulk on the same machine where agents will live (a dev box, a personal laptop, a server you're already SSH'd into).

## Install

```bash
cargo install --path .
```

Or build from source:

```bash
git clone https://github.com/frantufro/skulk.git
cd skulk
cargo build --release
# Binary is at target/release/skulk
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

# Create an agent reachable from the Claude Code mobile/web app
skulk new mobile-task --remote-control "Fix the login bug"

# Spin up an agent on a specific model
skulk new big-refactor --model opus "Untangle the auth middleware"

# Pass arbitrary extra flags through to Claude Code.
# Note the inner single quotes around Bash(...): --claude-args is typed into
# the remote shell by tmux, so shell metacharacters (parens, globs, $, ;, …)
# must be pre-quoted to reach Claude literally.
skulk new scoped --claude-args "--allowed-tools 'Bash(gh pr:*)'" "Triage open PRs"
```

By default Skulk launches Claude Code **without** `--remote-control`. Skulk's own commands (`connect`, `logs`, `send`, `disconnect`) talk to the agent through tmux directly and don't need it, and leaving it on triggers an upstream idle-death bug ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)) that kills long-running agents. Opt in with `--remote-control` when you want to drive an agent from your phone.

> **Heads-up:** agents launched with `--remote-control` currently die after **~20 minutes of inactivity** due to the upstream bug above. This is acceptable for interactive mobile-app use (you're driving the agent), but don't use `--remote-control` for long autonomous tasks — omit the flag and drive with `skulk send` / `skulk connect` instead.

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
| `skulk list` | List all running agents with status, uptime, and worktree path |
| `skulk new <name> [prompt]` | Create a new agent with its own worktree; optionally send an initial prompt |
| `skulk connect <name>` | Attach to an agent's live tmux session |
| `skulk logs <name>` | View an agent's terminal output |
| `skulk send <name> <prompt>` | Send a prompt to a running agent |
| `skulk ship <name>` | Push the agent's branch and open a PR with a Claude-authored description (requires `gh` and `claude` on the remote) |
| `skulk pull` | Update the base clone (`git pull --ff-only`) |
| `skulk destroy <name>` | Destroy an agent (session, worktree, and branch) |
| `skulk destroy-all` | Destroy all agents at once |
| `skulk gc` | Clean up orphaned sessions, worktrees, and branches |

## Per-Agent Setup (Init Hook)

Skulk runs an optional setup script inside each agent's tmux session before Claude starts — useful for `docker compose up`, migrations, dependency installs, mock services, etc.

**Convention:** put the script at `.skulk/init.sh` in your repo. Override the path with `init_script = "scripts/setup-agent.sh"` in `.skulk.toml` if you prefer.

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
| `--remote-control` | `new` | Launch Claude with `--remote-control` so the agent is accessible from the Claude Code mobile/web app. Off by default because of an upstream idle-death bug ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)) that kills agents after ~20 min of inactivity — see Quick Start |
| `--model <NAME>` | `new` | Pass `--model <name>` through to Claude Code (e.g. `opus`, `sonnet`, `claude-opus-4-7`). Restricted to `[A-Za-z0-9._-]`. |
| `--claude-args <ARGS>` | `new` | Extra flags appended to the Claude Code launch command. Typed into the remote shell by tmux, so shell metacharacters are re-evaluated — pre-quote for the inner shell (e.g. `--claude-args "--allowed-tools 'Bash(gh pr:*)'"`). |
| `--force` | `pull` | Hard-reset to `origin/main` instead of fast-forward |
| `--force` | `destroy`, `destroy-all` | Skip the confirmation prompt |
| `--follow`, `-f` | `logs` | Stream output in real time |
| `--lines`, `-l` | `logs` | Number of scrollback lines to show |
| `--dry-run` | `gc` | Preview what would be cleaned |

## Error Handling

Skulk gives you actionable diagnostics instead of raw SSH errors:

- **Connection refused** — check that SSH is running on the remote
- **Host key verification failed** — accept the host key first
- **Permission denied** — check your SSH key or config
- **Agent not found** — the named agent doesn't exist; use `skulk list` to see what's running
- **Base clone missing** — run `skulk init` to set up the remote server

Destructive operations (`destroy`, `destroy-all`) require confirmation unless `--force` is passed. If agent creation fails partway through (e.g., tmux session can't start), the worktree is automatically rolled back.

## Development

```bash
cargo fmt                                        # Format
cargo clippy -- -D warnings -W clippy::pedantic  # Lint (zero warnings)
cargo test                                       # Run all tests
```

The codebase is organized into focused modules, each with co-located tests:

```
src/
├── main.rs          CLI definition and command dispatch
├── io.rs            System boundary (real SSH, stdin) — excluded from coverage
├── error.rs         SkulkError enum and SSH error classification
├── ssh.rs           Ssh trait (injectable for testing)
├── config.rs        Config struct and .skulk/config.toml loading
├── util.rs          Validation, shell escaping, shared helpers
├── display.rs       Session types, table formatting, GC summary
├── inventory.rs     Single-roundtrip remote state gathering
├── testutil.rs      MockSsh and test builders (test-only)
└── commands/
    ├── init.rs      Interactive setup wizard and remote provisioning
    ├── list.rs      Agent listing and status display
    ├── new.rs       Agent creation with worktree isolation
    ├── pull.rs      Base clone updates
    ├── destroy.rs   Agent teardown (single and bulk)
    ├── interact.rs  Connect, logs, and send
    └── gc.rs        Orphan detection and cleanup
```

Everything is tested through an injectable `Ssh` trait with a `MockSsh` test double — no real SSH calls in the test suite.

## Contributing

Contributions are welcome. Please make sure `cargo fmt`, `cargo clippy -- -D warnings -W clippy::pedantic`, and `cargo test` all pass before submitting a PR.

## License

[MIT](LICENSE)
