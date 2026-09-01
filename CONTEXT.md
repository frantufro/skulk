# Skulk

Skulk runs coding agents on a remote server, each in its own tmux session and
git worktree, and ships an agent skill so a local agent drives that fleet the
same way a person does. This glossary fixes the words used across the CLI, the
skill, and the installers.

## Language

### Agent lifecycle

**Agent**:
One unit of delegated work on the remote, under a name the developer chooses. An
agent owns a session, a worktree, and a branch, all three sharing the qualified
name `<session_prefix><name>`.
_Avoid_: job, task, instance, worker, bot

**Session**:
The tmux session an agent's harness runs inside. Its lifetime is the agent's
running lifetime; killing it leaves the work in place.
_Avoid_: terminal, shell, window, pane

**Worktree**:
The private directory tree where an agent's harness edits files, attached to the
base clone as a git worktree. Agents never share one.
_Avoid_: copy, sandbox, folder, checkout

**Base clone**:
The single clone of the repository on the remote that every worktree hangs off,
at `base_path`. `skulk pull` refreshes it.
_Avoid_: origin, upstream, main repo, checkout

**Idle state**:
What an agent's harness is doing at this moment: `working`, `idle`, or
`stopped`. This is a separate question from which of the agent's resources
exist.
_Avoid_: status, health, progress, phase

**Archived agent**:
An agent whose session has been killed while its worktree and branch survive.
`skulk restart` brings one back.
_Avoid_: stopped, paused, suspended, dead

**Orphan**:
A session, worktree, or branch carrying the session prefix whose companions are
gone. `skulk gc` reaps them. A worktree that still has its branch is an
archived agent and survives gc.
_Avoid_: stale, leak, garbage, zombie

**Ship**:
Pushing an agent's branch to origin and opening a pull request from it.
_Avoid_: deliver, land, merge, deploy, release

**Fleet**:
All the agents standing on the remote for one project.
_Avoid_: pool, cluster, swarm, group

### Agent hosting

**Harness**:
The agent program skulk launches on the remote, named by the `harness` field of
`.skulk/config.toml`.
_Avoid_: agent, backend, runner, engine

**Host**:
A program on the developer's machine that discovers and loads skills. Skulk
targets OpenCode and Claude Code.
_Avoid_: client, editor, tool, IDE

### Skill distribution

**Skill**:
A directory containing a `SKILL.md` whose YAML frontmatter carries a `name` and
a `description`, which a host loads to teach a model a capability.
_Avoid_: prompt, instructions, rule, agent

**Scope**:
Which directory an installation writes to. `global` is the host's user config
directory; `project` is a directory inside the current repository.
_Avoid_: level, location, target

**Installer**:
`bin/install.mjs`, published to npm as `@frantufro/skulk`, which places skills
in a host's directory and fetches the binary when one is missing.
_Avoid_: plugin, bootstrapper, setup script

**Binary**:
The compiled `skulk` executable that skill instructions invoke.
_Avoid_: CLI, tool, program

## Relationships

- An **Agent** owns one **Session**, one **Worktree**, and one branch, all
  sharing its qualified name
- Every **Worktree** hangs off the **Base clone**
- An **Archived agent** has kept its **Worktree** and branch after its
  **Session** was killed
- An **Orphan** is a **Session**, **Worktree**, or branch whose companions are
  gone
- The **Fleet** is every **Agent** on one remote for one project
- A **Host** loads one or more **Skills** and invokes the **Binary**
- The **Binary** launches one **Harness** per agent on the remote
- **Host** and **Harness** are independent: either may be OpenCode or Claude
  Code, in any of the four combinations
- The **Installer** writes **Skills** into one **Host** at one **Scope**

## Example dialogue

> **Dev:** "If I install the skill into OpenCode, do my agents become OpenCode
> agents?"
> **Domain expert:** "No. OpenCode is the **Host** there — it reads the skill
> and runs `skulk`. Which **Harness** the agents run is the `harness` field in
> `.skulk/config.toml`, and it still says `claude` until you change it."

## Flagged ambiguities

- "OpenCode support" was used to mean both **Host** and **Harness** — resolved:
  these are independent roles, and the same program can occupy both at once.
- "Session" carries two meanings across the material skulk touches: the tmux
  session skulk creates and names, and the harness's own conversation, which
  `skulk upload` and `skulk download` carry between machines. This glossary
  reserves **Session** for the tmux one and spells out "conversation" for the
  other.
