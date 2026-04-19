---
status: DONE
implemented: dc19793
---

Add two new flags to `skulk new` for loading the initial prompt from external sources:

- **`skulk new <name> --github <issue-id>`** — fetch a GitHub issue (title + body + all comments) from the remote via `gh issue view <id> --json title,body,comments`, format into a wrapped prompt, and send to the agent
- **`skulk new <name> --from <text-file>`** — read a local text file, wrap its contents into a prompt, and send to the agent

Agent name is still passed explicitly (no auto-derivation). The flag replaces the current optional positional `prompt` argument.

**Prompt wrapper for `--from`** (starting draft, tune in practice):

```
You've been assigned a task. Read it carefully, then ask me clarifying questions one at a time before you start implementing.

You're working in a dedicated git worktree on branch `{branch}` — feel free to commit freely; the branch is isolated.

---
{file contents}
---
```

**Prompt wrapper for `--github`**:

```
You've been assigned GitHub issue #{id}. The full issue and all comments are below. Read them carefully, then ask me clarifying questions one at a time before you start implementing.

You're working in a dedicated git worktree on branch `{branch}` — feel free to commit freely. You have `gh` available if you need to interact with the issue further.

--- Issue #{id}: {title} ---
{body}

--- Comments ({n}) ---
{author} ({date}):
{comment body}
...
```

**Implementation notes**:
- For `--github`: requires `gh` installed and authenticated on the remote. `skulk init` should add a check/install step for this (separate concern, mention in task).
- For `--from`: file is read locally on the user's machine, contents shipped over SSH.
- Detect missing/unauthenticated `gh` and give a clear error.
- Cross-repo issue support (e.g. `--github owner/repo#123`) — leave as a follow-up if needed.

**Out of scope** (intentionally — task management is not skulk's concern):
- No state tracking of issue/file across runs
- No automatic issue commenting/closing on PR merge
- No "task done" detection
