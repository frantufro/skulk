# Skulk

Skulk runs coding agents on a remote server, each in its own tmux session and
git worktree, and ships an agent skill so a local agent drives that fleet the
same way a person does. This glossary fixes the words used across the CLI, the
skill, and the installers.

## Language

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
