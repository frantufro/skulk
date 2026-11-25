# `skulk`

Dead simple agent management over SSH. For humans and agents alike.

`skulk` spins up [Claude Code](https://docs.claude.com/en/docs/claude-code) agents on a remote server — each in its own tmux session and git worktree, fully isolated, zero conflicts.

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

I wanted my agents to keep working when my laptop is closed. `skulk` runs them on a remote server in tmux sessions, each in its own git worktree. I check in from my laptop, or from my phone when the agent is launched with [`--remote-control`](#2-spin-up-agents), read the diffs, ship what's good.

Works as a CLI, in scripts, or driven by another Claude Code session through the [plugin](#claude-code-plugin).

## Requirements

**Local machine:** macOS or Linux with an OpenSSH client.

**Remote server:** Debian-based Linux (Ubuntu, Debian, etc.) with SSH access and key-based auth. `skulk init` installs everything else (tmux, git, Claude Code). Other distros may work but are not officially supported yet.

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
| `harness` | Agent harness to use: `claude` (default) or `opencode` |

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

By default `skulk` launches Claude Code **without** `--remote-control`. `skulk`'s own commands (`connect`, `logs`, `send`) talk to the agent through tmux directly, and leaving `--remote-control` on triggers an upstream idle-death bug ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)) that kills agents after ~20 minutes of inactivity. Only opt in when you want to drive an agent from the Claude Code mobile/web app, and don't use it for long autonomous tasks.

### 3. Monitor and interact

```bash
# See what's running
skulk list

# Detailed view of one agent (status, commits ahead, files changed, uptime)
skulk status fix-bug

# View an agent's terminal output
skulk logs fix-bug

# Follow output in real time (like tail -f)
skulk logs fix-bug --follow

# View scrollback history
skulk logs fix-bug --lines 500

# Output of another tmux window, e.g. a dev server the init hook started
skulk logs fix-bug --window server --follow

# Review the work so far
skulk diff fix-bug --stat      # summary of files changed
skulk diff fix-bug             # full diff
skulk git-log fix-bug          # commits on the agent's branch

# Attach to an agent's live tmux session (interactive)
skulk connect fix-bug
# Detach with Ctrl+B then D

# Send a follow-up prompt to a running agent
skulk send fix-bug "Actually, also add a test for the edge case"

# Or send a longer prompt from a file
skulk send fix-bug --from ./followup.md

# Block until the agent finishes its current turn (useful in scripts)
skulk wait fix-bug
skulk wait --all --timeout 600   # wait for every agent, cap at 10 min
```

### 4. Ship the work

```bash
# Push the agent's branch to origin
skulk push fix-bug

# Push and open a PR with a Claude-authored description
skulk ship fix-bug
```

### 5. Pause, resume, clean up

```bash
# Archive an agent — kill its tmux session but keep the worktree and branch
skulk archive fix-bug

# Bring an archived or crashed agent back with a fresh Claude session
skulk restart fix-bug

# Update the base clone on the remote (fast-forward origin/main → base_path)
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
| **Setup** | |
| `skulk init` | Interactive setup wizard — generates config and optionally provisions the remote server |
| `skulk doctor` | Health check — verify SSH, tools, base clone, and worktree directory |
| `skulk pull` | Update the base clone (`git pull --ff-only`) |
| **Create** | |
| `skulk new <name>` | Create a new agent with its own worktree (`--from`, `--github`, `--model`, `--claude-args`, `--remote-control`) |
| **Observe** | |
| `skulk list` | List all agents with status, uptime, and worktree path |
| `skulk status <name>` | Detailed single-agent view: status, commits ahead, files changed, uptime |
| `skulk logs <name>` | View an agent's terminal output (`-f` to follow, `-l` for scrollback, `-w <window>` for another window of its session) |
| `skulk wait <name>` | Block until the agent is idle (`--all` for all agents, `--timeout <secs>` to cap the wait) |
| **Interact** | |
| `skulk connect <name>` | Attach to an agent's live tmux session |
| `skulk disconnect <name>` | Detach all clients from an agent's session |
| `skulk send <name> <prompt>` | Send a prompt to a running agent (`--from` to read from a file) |
| `skulk restart-window <name> <window>` | Restart one window of the agent's session (e.g. the dev server in `server`) with the command it was created with — see [Init Hook](#per-agent-setup-init-hook) |
| **Review** | |
| `skulk diff <name>` | Show git diff against the default branch (`--stat`, `--name-only`) |
| `skulk git-log <name>` | Show commits on the agent's branch not in the default branch |
| `skulk transcript <name>` | Dump full tmux scrollback (`--output` to write to a file) |
| **Ship** | |
| `skulk push <name>` | Push the agent's branch to origin |
| `skulk ship <name>` | Push and open a PR with a Claude-authored description |
| **Lifecycle** | |
| `skulk archive <name>` | Kill tmux session but keep worktree and branch intact |
| `skulk restart <name>` | Restart an archived or crashed agent in its existing worktree |
| `skulk destroy <name>` | Destroy an agent (session, worktree, and branch) |
| `skulk destroy-all` | Destroy all agents at once |
| `skulk gc` | Clean up orphaned tmux sessions, worktrees, and branches. Scoped to the project's `session_prefix`; preserves running and archived agents. |
| `skulk completions <shell>` | Print a shell completion script (`bash`, `zsh`, or `fish`) to stdout |

## Per-Agent Setup (Init Hook)

`skulk` runs an optional setup script inside each agent's tmux session before Claude starts — useful for `docker compose up`, migrations, dependency installs, mock services, etc.

**Convention:** put the script at `.skulk/init.sh` in your repo. Override the path with `init_script = "scripts/setup-agent.sh"` in `.skulk/config.toml` if you prefer.

**Project env file:** `.skulk/.env` lives locally (gitignored — `skulk init` adds the entry automatically) and almost always contains secrets. On `skulk new`, `skulk` copies it to the agent's worktree at `<worktree>/.env` so dotenv-aware project tooling picks it up, and `skulk` also `source`s it before running `init.sh` so both the script **and Claude itself** see the same vars (e.g. `$DATABASE_URL` for migrations, `$GITHUB_TOKEN` for tools Claude uses).

> ⚠️ **Security:** shipping `.skulk/.env` sends your local secrets to the remote server. Review what's in it before running `skulk new`, especially on shared hosts.

**Env vars passed to `init.sh`:**

| Variable | Example |
|----------|---------|
| `SKULK_AGENT_NAME` | `auth-refactor` |
| `SKULK_SESSION` | `my-project-auth-refactor` |
| `SKULK_BRANCH` | `my-project-auth-refactor` |
| `SKULK_WORKTREE` | absolute path to the worktree |

**Failure handling — hard fail:** if `init.sh` exits non-zero, Claude does not start. The tmux session stays open with the error visible — run `skulk connect <name>` to investigate. For per-step opt-outs, use the usual shell idiom: `risky_command || true`.

**Long-running services in their own window:** to keep a dev server running next to the agent, have `init.sh` open it in a separate tmux window *with the command*, so tmux remembers what to restart:

```bash
tmux new-window -d -t "$SKULK_SESSION" -n server "cd '$SKULK_WORKTREE' && bin/dev"
# optional: keep the window and its last output when the server exits
tmux set-option -w -t "$SKULK_SESSION:=server" remain-on-exit on
```

Then `skulk logs <name> --window server` shows its output (`--follow` and `--lines` work too), and `skulk restart-window <name> server` restarts it without re-running `init.sh`. `restart-window` runs `tmux respawn-window -k`, which re-runs the window's *original* command — a window opened as a plain shell with `bin/dev` typed into it via `send-keys` comes back as an empty shell. Don't name the window `agent`: that's the window Claude runs in.

`skulk init` writes `.skulk/init.sh.example` — rename it to `.skulk/init.sh` and customize to enable.

## Claude Code Plugin

`skulk` ships a Claude Code plugin that teaches Claude how to drive `skulk`
directly. Install it and your Claude Code session becomes an orchestrator:
ask it to spin up agents on a list of tasks, check in on the fleet, review
their diffs, and ship the ones that are ready — it'll run the right `skulk`
commands for you.

Install via the plugin marketplace. Type these as slash commands inside a
Claude Code session:

```
/plugin marketplace add frantufro/claude-plugins
/plugin install skulk@frantufro-plugins
```

The plugin contributes a `skulk-agent-management` skill that covers the
full agent lifecycle (create, monitor, interact, review, ship, clean up).

## OpenCode Skill

The same skill is available to OpenCode through npm:

```bash
npx @frantufro/skulk install
```

That writes the `skulk-agent-management` skill into
`~/.config/opencode/skills/`, and downloads the `skulk` binary into
`~/.local/bin` when `skulk` is absent from your `PATH`. The download's SHA-256
is checked against the checksum published with the release before anything is
installed. Once the binary is in place, `skulk update` handles upgrades.

Options:

| Flag | Effect |
| --- | --- |
| `--project` | install into `./.opencode/skills` for one repository |
| `--force` | overwrite a modified `SKILL.md` without asking |
| `--skip-existing` | keep a modified `SKILL.md` and exit successfully |
| `--no-binary` | install the skill alone |

The skill this installs is the one the Claude Code plugin contributes, packed
verbatim from `claude-plugin/skills/`. Whichever program loads the skill, the
`harness` setting still decides what runs on the remote, so an OpenCode session
can drive Claude Code agents and a Claude Code session can drive OpenCode ones.

## Alternative Harnesses

By default, `skulk` uses [Claude Code](https://docs.claude.com/en/docs/claude-code) as the agent harness. You can switch to [OpenCode](https://opencode.ai) by setting the `harness` field in `.skulk/config.toml`:

```toml
harness = "opencode"
```

If omitted, `harness` defaults to `"claude"`.

### Installing OpenCode on the Remote Server

OpenCode must be installed on the remote server before using the `opencode` harness. Follow the [OpenCode installation guide](https://opencode.ai/docs/install) for your remote's OS, or run this one-liner for Linux (Debian-based):

```bash
curl -fsSL https://opencode.ai/install | bash
```

Verify the installation by running `opencode --version` over SSH on the remote.

### OpenCode-Specific Behavior

- **`skulk wait` limitation:** OpenCode does not have an equivalent to Claude Code's `UserPromptSubmit` event, so `skulk wait` may return early right after sending a prompt, before the agent finishes its turn.
- **`--model` flag format:** When using OpenCode, the `--model` flag requires the `provider/model` format (e.g. `anthropic/claude-opus-4-7`), not just the model name.
- **`--remote-control` ignored:** The `--remote-control` flag is Claude-only and will be ignored when using the OpenCode harness.

## How It Works

```
Local                          Remote Server
─────                          ─────────────
skulk init ──────SSH──►  Tests connectivity
                         Installs tmux, git, claude (if missing)
                         Clones repo to base_path
                         Creates worktree_base directory

skulk new auth ──SSH──►  git worktree add <worktree_base>/<prefix>auth
                         tmux new-session -d -s <prefix>auth
                         (starts claude in the worktree)

skulk send auth ──SSH──► tmux send-keys "your prompt" Enter
                         (captures pane before/after to verify delivery)

skulk connect auth ──SSH──► tmux attach -t <prefix>auth
                            (interactive terminal, Ctrl+B D to detach)

skulk destroy auth ──SSH──► tmux kill-session -t <prefix>auth
                            git worktree remove <prefix>auth
                            git branch -D <prefix>auth
```

Each agent is a tmux session running Claude Code inside its own git worktree. Worktrees share the same `.git` directory as the base clone but have independent working trees and branches — so agents can edit files simultaneously without stepping on each other.

## Shell Completions

Skulk ships tab completion for `bash`, `zsh`, and `fish`. Completions cover every subcommand and flag, and also tab-complete **live agent names** for commands that take one — `skulk destroy <Tab>` suggests the agents currently running on your remote.

Pick your shell and source the output from your shell's config:

**bash** — add to `~/.bashrc`:

```bash
source <(skulk completions bash)
```

**zsh** — add to `~/.zshrc` (after `compinit`):

```zsh
source <(skulk completions zsh)
```

**fish** — write once into fish's completions dir:

```fish
skulk completions fish > ~/.config/fish/completions/skulk.fish
```

Dynamic agent-name completion shells out to `skulk list` at Tab time. If `.skulk/config.toml` isn't set up yet, or the remote is unreachable, it silently returns nothing and falls back to static completion — so it's safe to source unconditionally.

## Agent Names

Names may contain letters (upper or lowercase), digits, hyphens, underscores, and forward slashes. 1-100 characters. Cannot start with `-` or `/`, end with `/`, or contain `//`.

```
skulk new my-feature      # valid
skulk new fix-123         # valid
skulk new My_Feature      # valid
skulk new feat/add-thing  # valid
skulk new -bad-name       # invalid (leading hyphen)
```

## Flags

| Flag | Scope | Description |
|------|-------|-------------|
| `--no-color` | Global | Disable colored output (also respects `NO_COLOR` env var) |
| `--json` / `--human` | Global | Machine-readable JSON or human text output; overrides `output_format` in the config (see [JSON Output](#json-output)) |
| `--config <PATH>` | Global | Use this config file, or the project directory holding `.skulk/config.toml`, instead of searching upward from the current directory |
| `--from <FILE>` | `new`, `send` | Load the prompt from a local text file instead of the positional argument |
| `--github <ISSUE_ID>` | `new` | Load the prompt from a GitHub issue (title, body, comments) via `gh` on the remote. Mutually exclusive with `--from`. |
| `--remote-control` | `new` | Launch Claude with `--remote-control` so the agent is accessible from the Claude Code mobile/web app. Off by default because of an upstream idle-death bug ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)) that kills agents after ~20 min of inactivity — see Quick Start |
| `--model <NAME>` | `new` | Pass `--model <name>` through to Claude Code (e.g. `opus`, `sonnet`, `claude-opus-4-7`). Restricted to `[A-Za-z0-9._-]`. |
| `--claude-args <ARGS>` | `new` | Extra flags appended to the Claude Code launch command. Typed into the remote shell by tmux, so shell metacharacters are re-evaluated — pre-quote for the inner shell (e.g. `--claude-args "--allowed-tools 'Bash(gh pr:*)'"`). |
| `--stat` | `diff` | Summary of changed files (insertions/deletions), mutually exclusive with `--name-only` |
| `--name-only` | `diff` | Paths of changed files only |
| `--follow`, `-f` | `logs` | Stream output in real time |
| `--lines`, `-l` | `logs` | Number of scrollback lines to show |
| `--window <WINDOW>`, `-w` | `logs` | Read this tmux window of the agent's session (e.g. `server`) instead of the agent's own window |
| `--output <FILE>`, `-o` | `transcript` | Write transcript to this file instead of stdout |
| `--all` | `wait` | Wait for every running agent instead of one |
| `--timeout <SECS>` | `wait` | Maximum seconds to wait before giving up (default: 1800; applies per agent with `--all`) |
| `--force` | `pull` | Hard-reset to `origin/main` instead of fast-forward |
| `--force` | `destroy`, `destroy-all` | Skip the confirmation prompt |
| `--dry-run` | `gc` | Preview what would be cleaned |

## JSON Output

With `--json` (or `output_format = "json"` in the config), each command prints
one JSON object on stdout when it succeeds. Every object carries `"v": 1`, the
schema version, which changes only when a payload changes in a way that breaks
consumers.

```bash
skulk --json --config ~/code/my-project list
# {"agents":[{"branch":"skulk-fix-bug","name":"fix-bug","status":"idle",...}],"v":1}
```

On failure, skulk exits non-zero, prints nothing on stdout, and prints one error
object on stderr:

```json
{"v":1,"error":"skulk destroy: No agent 'ghost' found.","code":"not_found"}
```

`code` is one of `validation`, `not_found`, `ssh_exec`, `ssh_failed`,
`diagnostic`, `update_failed`, `config` (the config file could not be found
or loaded), or `json_serialization` (a result could not be encoded; it never
reaches stdout).

| Command | Success payload (besides `v`) |
|---------|-------------------------------|
| `list` | `agents`: array of `{name, status, branch, uptime_secs, session_created_epoch}` |
| `new`, `replay` | `ok`, `agent`, `branch`, `session`, `worktree`, `harness`, `model`, `remote_control`, `prompt` (`delivered` / `failed` / `none`), `warnings` |
| `restart` | `ok`, `agent`, `branch`, `session`, `worktree`, `harness`, `model`, `remote_control` |
| `archive` | `ok`, `agent`, `branch`, `worktree`, `reason`, `warnings` |
| `destroy` | `ok`, `agent`, `cleaned`: list of `session` / `worktree` / `branch` |
| `destroy-all` | `ok`, `agents`: names of the destroyed agents |
| `push` | `ok`, `agent`, `branch`, `remote`, `output` |
| `pull` | `ok`, `base_path`, `branch`, `force`, `output` |
| `ship` | `ok`, `agent`, `pr_url` |
| `wait` | `ok`, `agent`, `status`, or `reason` on timeout |
| `status`, `gc`, `logs`, `diff`, `restart-window` | command-specific fields |

`destroy` and `destroy-all` need `--force` with `--json`, since nobody is there
to answer the confirmation prompt. If they leave anything behind, they fail
with a `diagnostic` error naming what is left. A `wait` that times out is
the one failure reported on stdout: `{"ok": false, "reason": "timeout", ...}`
with exit code 1.

> **Breaking change in the `v: 1` contract:** `list` used to print a bare JSON
> array. It now prints `{"v": 1, "agents": [...]}`. Read `.agents` instead of
> the top-level array.

## Error Handling

`skulk` classifies common SSH failures and surfaces actionable suggestions instead of raw stderr:

- **Connection refused / timed out** — check that SSH is running and reachable on the remote
- **Could not resolve hostname** — check your `~/.ssh/config` or DNS
- **Host key verification failed** — accept the host key first
- **Permission denied** — check your SSH key, agent, or `~/.ssh/config`
- **Agent not found** — the named agent doesn't exist; run `skulk list` to see what's running
- **Base clone missing / remote tools missing** — run `skulk init` to provision the remote, or `skulk doctor` to diagnose an existing setup

Run `skulk doctor` any time for a full health check (SSH reachability, required tools on the remote, base clone, worktree directory).

Destructive operations (`destroy`, `destroy-all`) require confirmation unless `--force` is passed, and exit non-zero if any session, worktree or branch could not be removed. If agent creation fails partway through (e.g., tmux session can't start), the worktree is rolled back automatically.

## Contributing

Contributions welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
ground rules, project layout, and release workflow.

## License

[MIT](LICENSE)
