---
created: 2026-06-25
---

# Add skulk upload command — transfer local branch and Claude session to a remote agent

## Context and Goal

`skulk upload` lets a user hand off their current local work to a skulk agent on the remote server. It transfers two things:
1. The current git branch (committed state) to the remote host via git bundle
2. The Claude Code conversation history (JSONL files at `~/.claude/projects/<encoded>/`) to the corresponding path on the remote

After upload, a new tmux session starts on the remote running Claude in the agent's worktree — the agent can immediately resume the conversation.

**This task depends on:**
- The name validation relaxation (task `relax-agent-name-validation-to-allow-uppercase-underscores-and-slashes`) being merged — agents can now have names like `feat/add-upload`
- The `claude_project_dir_name` and `remote_claude_project_dir_command` helpers from `add-claude-code-project-path-encoding-helper`

## Command Interface

```
skulk upload [--to <agent-name>] [--force]
```

**Modes:**
- No `--to`: creates a NEW agent. The agent name defaults to the current local branch name (which is valid under the new naming rules). Fails if an agent with that name already exists (same check as `skulk new`).
- `--to <name>`: uploads into an EXISTING agent (it must already have a worktree on the remote). Fails if the remote agent already has Claude Code session files at `~/.claude/projects/<remote-hash>/`, unless `--force` is given.

**Preconditions:**
- Local git must be clean: `git status --porcelain` must return empty output. If there are uncommitted changes, print an error and exit: `"Cannot upload: local working tree has uncommitted changes. Commit or stash first."`
- Current branch must not be the detached HEAD state: `git branch --show-current` must return a non-empty string.

## Architecture: Local Operations

`skulk upload` is unique in that it needs to run **local** commands (git, filesystem), not just remote SSH commands. This follows the pattern established in `src/commands/init.rs` which injects a `run_local_command: &dyn Fn(&str) -> Result<String, String>` for testability.

Create a `LocalOps` trait in `src/commands/upload.rs`:

```rust
pub(crate) trait LocalOps {
    /// Run `git status --porcelain` in the project root. Returns stdout.
    fn git_status(&self) -> Result<String, SkulkError>;
    /// Run `git branch --show-current`. Returns the branch name (trimmed).
    fn git_current_branch(&self) -> Result<String, SkulkError>;
    /// Create a git bundle of `branch` at `dest_path`. Uses: `git bundle create <dest> <branch>`.
    fn create_git_bundle(&self, branch: &str, dest: &std::path::Path) -> Result<(), SkulkError>;
    /// Return the path to the local `~/.claude/projects/` directory.
    fn claude_projects_dir(&self) -> std::path::PathBuf;
    /// List all files (not directories) inside `dir`. Returns empty vec if dir doesn't exist.
    fn list_dir_files(&self, dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, SkulkError>;
    /// Return the absolute path of the local project root (the directory containing `.skulk/`).
    fn project_root(&self) -> std::path::PathBuf;
    /// Return a temporary file path for the git bundle (e.g. in the system temp dir).
    fn temp_bundle_path(&self) -> std::path::PathBuf;
    /// Remove a local file (used to clean up the temp bundle after upload).
    fn remove_file(&self, path: &std::path::Path) -> Result<(), SkulkError>;
}
```

The real implementation (`RealLocalOps`) lives in `src/io.rs` (system boundary). A `MockLocalOps` lives in `src/testutil.rs`.

## Git Transfer via Bundle

Instead of pushing through GitHub/origin, we transfer git commits directly to the skulk remote host using `git bundle`:

1. **Local**: `git bundle create /tmp/skulk-upload-<name>.bundle <local_branch>` — packages all commits on the branch
2. **Transfer**: `ssh.upload_file(&bundle_path, &remote_bundle_path)` where `remote_bundle_path = "/tmp/skulk-upload-<session_name>.bundle"` — uses existing `Ssh::upload_file` (scp)
3. **Remote**: `git -C {base_path} fetch {remote_bundle_path} {local_branch}:{branch_name}` — imports commits from the bundle into the remote git repo as branch `{session_prefix}{agent_name}`
4. **Remote**: create worktree pointing at the branch (see below)
5. **Cleanup remote**: `rm -f {remote_bundle_path}`
6. **Cleanup local**: remove the temp bundle file

This approach requires no shared git remote (no GitHub needed) and uses skulk's existing `Ssh::upload_file` infrastructure.

## Upload Flow (step by step)

Implement `cmd_upload` in `src/commands/upload.rs`:

```
fn cmd_upload(ssh, local, name, to_existing, force, cfg) -> Result<(), SkulkError>
```

**Step 1: Check git clean state**
Call `local.git_status()`. If result is non-empty, return error: `"Cannot upload: local working tree has uncommitted changes. Commit or stash first."`

**Step 2: Get current branch**
Call `local.git_current_branch()`. If empty (detached HEAD), return error: `"Cannot upload: not on a named branch (detached HEAD). Check out a branch first."`
Let `local_branch` = the branch name.

**Step 3: Determine agent name**
- If `--to <name>` was provided: `agent_name = name`
- Otherwise: `agent_name = local_branch` (the branch name IS the agent name)
Call `validate_name(&agent_name)?`.

**Step 4: Validate remote state**
Fetch inventory with `fetch_inventory(ssh, cfg)`.
- If `--to <existing>`:
  - Verify the agent has a worktree (`inv.worktrees.contains_key(&session_name)`)
  - If no worktree: return error `"Agent '{agent_name}' has no worktree on the remote. Run `skulk new {agent_name}` first."`
  - Check for existing Claude session files on remote (see Step 7a below); if they exist and `--force` is false, return error
- If no `--to` (new agent mode):
  - Verify name is unique: no session AND no worktree with that name (same as `create_agent_with_prompt`)
  - If conflict: return same error messages as `skulk new`

**Step 5: Create git bundle locally**
```rust
let bundle_path = local.temp_bundle_path(); // e.g. /tmp/skulk-upload-<name>.bundle
local.create_git_bundle(&local_branch, &bundle_path)?;
```

**Step 6: Transfer bundle to remote**
```rust
let remote_bundle = format!("/tmp/skulk-upload-{}.bundle", AgentRef::new(&agent_name, cfg).session_name());
ssh.upload_file(&bundle_path, &remote_bundle)
    .context("Failed to transfer git bundle to remote")?;
```

**Step 7a: Check for existing remote JSONL (for --to mode)**
Run on remote: `test -d ~/.claude/projects/$(cd {worktree_path} && pwd | tr '/' '-') && echo exists`
If it returns `"exists"` and `--force` is false: clean up bundle, return error: `"Agent '{agent_name}' already has a Claude session on the remote. Use --force to overwrite."`

**Step 7b: Import branch from bundle on remote (new agent mode)**
Run on remote:
```
cd {base_path} && git fetch {remote_bundle} {local_branch}:{branch_name}
```
where `branch_name = AgentRef::new(&agent_name, cfg).branch_name()` = `{session_prefix}{agent_name}`.

**Step 7c: Create remote worktree (new agent mode)**
Run on remote: `git worktree add {worktree} {branch_name}` (no `-b` flag since the branch already exists from the fetch step). Install Claude Code hooks in the worktree — reuse `agent_create_worktree_command` from `src/commands/new.rs` for the hooks-only variant, OR just use the same command but with `git worktree add {worktree} {branch_name}` instead of `git worktree add -b {branch_name} {worktree} {default_branch}`.

**Note on worktree creation command:** Look at `agent_create_worktree_command` in `src/commands/new.rs`. It creates the worktree AND installs hooks (Claude Code `settings.local.json` or OpenCode plugin). For upload, you need to do the same hook installation but against an existing branch. Factor out the hook installation into a separate helper or call the existing command with a modified worktree creation step.

**Step 8: Clean up remote bundle**
Run on remote (non-fatal): `rm -f {remote_bundle}`

**Step 9: Clean up local bundle**
Call `local.remove_file(&bundle_path)` (non-fatal, warn on error).

**Step 10: Transfer Claude session files**
Compute local project dir:
```rust
let local_root = local.project_root();
let encoded_local = claude_project_dir_name(&local_root.to_string_lossy());
let local_session_dir = local.claude_projects_dir().join(&encoded_local);
```

Compute remote project dir (requires the worktree to now exist on remote):
```rust
let worktree_path = AgentRef::new(&agent_name, cfg).worktree_path(cfg);
let remote_encoded = ssh.run(&remote_claude_project_dir_command(&worktree_path))?;
let remote_session_dir = format!("~/.claude/projects/{remote_encoded}");
```

Create remote dir and upload each JSONL file:
```rust
ssh.run(&format!("mkdir -p {remote_session_dir}"))?;
let files = local.list_dir_files(&local_session_dir)?;
for file in files {
    let filename = file.file_name().unwrap().to_string_lossy();
    let remote_path = format!("{remote_session_dir}/{filename}");
    ssh.upload_file(&file, &remote_path)?;
}
```

If local session dir doesn't exist (no Claude history), skip silently — the agent starts with no history, which is fine.

**Step 11: Create tmux session**
Reuse `agent_create_tmux_command` from `src/commands/new.rs` with `remote_control=false`, `model=None`, `claude_args=None` (or thread through flags if we add them later).
```rust
ssh.run(&agent_create_tmux_command(&agent_name, cfg, false, None, None))?;
```

**Step 12: Print success message**
```
Uploaded '{local_branch}' to agent '{agent_name}' on {host}.
  Connect: skulk connect {agent_name}
  Watch:   skulk logs {agent_name} --follow
```

## New Files to Create

- `src/commands/upload.rs` — the command implementation
  - `pub(crate) trait LocalOps { ... }` 
  - `pub(crate) fn cmd_upload(...) -> Result<(), SkulkError>`
  - Co-located `#[cfg(test)] mod tests { ... }` with `MockLocalOps`

## Files to Modify

- **`src/main.rs`**: Add `Upload` variant to `Commands` enum and dispatch to `upload::cmd_upload`. The `Upload` variant needs:
  - `to: Option<String>` for `--to <agent-name>`
  - `force: bool` for `--force`
  No positional arguments (agent name is derived from branch or `--to`).
  Add `mod upload;` to the `commands` module.

- **`src/commands/mod.rs`** (or wherever modules are declared): add `pub(crate) mod upload;`

- **`src/io.rs`**: Add `RealLocalOps` implementing `LocalOps` using `std::process::Command` for git operations and `std::fs` for filesystem operations. Pass `RealLocalOps` to `cmd_upload` from the dispatch in `main()`.

- **`src/testutil.rs`**: Add `MockLocalOps` with configurable responses for each method.

## Tests

Write unit tests in `src/commands/upload.rs` using `MockSsh` and `MockLocalOps`. Key scenarios:

- `upload_refuses_when_dirty_working_tree`: `git_status()` returns non-empty → error contains "uncommitted changes"
- `upload_refuses_on_detached_head`: `git_current_branch()` returns empty string → error contains "detached HEAD"
- `upload_creates_new_agent_from_branch`: happy path for new-agent mode; verify SSH calls include `git fetch`, `git worktree add`, tmux create, and mkdir for session dir
- `upload_to_existing_refuses_without_force_when_session_exists`: `--to` mode with existing JSONL → error contains "already has a Claude session"
- `upload_to_existing_with_force_overwrites`: `--force` allows overwriting existing session
- `upload_skips_session_transfer_when_no_local_history`: `list_dir_files()` returns empty → no upload_file calls for JSONL
- `upload_bundle_cleanup_is_nonfatal`: `remove_file()` fails → `cmd_upload` still returns `Ok`

## Verification

```
cargo fmt
cargo clippy -- -D warnings -W clippy::pedantic
cargo test
```
