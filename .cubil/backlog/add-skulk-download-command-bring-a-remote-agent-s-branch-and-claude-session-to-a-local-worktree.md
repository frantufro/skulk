---
created: 2026-06-25
---

# Add skulk download command — bring a remote agent's branch and Claude session to a local worktree

## Context and Goal

`skulk download <agent>` is the reverse of `skulk upload`. It pulls a remote agent's work back to the local machine so development can continue locally (or on a different machine). It transfers:
1. The remote agent's git branch to a new **local git worktree** at `../<branch-name>` (sibling of the current project directory)
2. The remote agent's Claude Code conversation files to the corresponding local path under `~/.claude/projects/`

After download, the remote agent is **auto-archived** (tmux session killed, worktree and branch preserved on remote) with the reason `"downloaded to <hostname>"`.

**This task depends on:**
- `relax-agent-name-validation-to-allow-uppercase-underscores-and-slashes` — agents can have slash names like `feat/add-upload`
- `add-optional-reason-flag-to-skulk-archive` — `cmd_archive` now accepts a reason parameter; download uses it
- `add-claude-code-project-path-encoding-helper` — `claude_project_dir_name` and `remote_claude_project_dir_command` helpers

## Command Interface

```
skulk download <agent-name> [--force]
```

- `<agent-name>`: required positional. Must be a valid agent name that exists on the remote.
- `--force`: overwrite local JSONL files if they already exist at the destination path.

## Preconditions

The local git repository (current directory) must be clean: `git status --porcelain` must return empty output. If there are uncommitted changes, fail with: `"Cannot download: local working tree has uncommitted changes. Commit or stash first."`

## Local Worktree Path

The local worktree is created at `../<branch-name>` where `<branch-name>` is the agent's fully-qualified branch name (i.e., `{session_prefix}{agent-name}`).

Example: if `session_prefix = "skulk-"` and `agent_name = "feat/add-upload"`, the branch name is `skulk-feat/add-upload` and the local worktree is created at `../skulk-feat/add-upload` (a sibling directory of the current project).

Note: the path `../skulk-feat/add-upload` creates an `add-upload` directory inside a `../skulk-feat/` directory — this is fine and follows from git's branch namespacing conventions.

If the local worktree path already exists, fail with: `"Cannot download: local path '../<branch-name>' already exists. Use --force to overwrite."` With `--force`, delete the existing directory before creating the worktree.

## Ssh::download_file — New Method on the Ssh Trait

The `download` command needs to copy files FROM the remote TO local. The existing `Ssh::upload_file` only goes local→remote. Add a new method:

**`src/ssh.rs`:**
```rust
/// Copy a remote file to a local path.
///
/// Used to retrieve Claude Code session files from a remote agent's
/// `~/.claude/projects/` directory.
fn download_file(&self, remote_path: &str, local_path: &Path) -> Result<(), SkulkError>;
```

**`src/io.rs` — `RealSsh::download_file`:**
```rust
fn download_file(&self, remote_path: &str, local_path: &Path) -> Result<(), SkulkError> {
    let local = is_localhost(&self.host);
    let output = if local {
        let local_str = local_path.to_string_lossy();
        let cmd = format!("cp {} '{}'", remote_path, shell_escape(&local_str));
        ProcessCommand::new("sh").args(["-c", &cmd]).output()
    } else {
        let src = format!("{}:{}", self.host, remote_path);
        ProcessCommand::new("scp")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .arg(&src)
            .arg(local_path)
            .output()
    }
    .map_err(|e| {
        if !local && e.kind() == std::io::ErrorKind::NotFound {
            SkulkError::Diagnostic {
                message: "scp command not found.".into(),
                suggestion: "Install OpenSSH.".into(),
            }
        } else {
            SkulkError::SshExec(e.to_string())
        }
    })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if local {
            Err(SkulkError::SshFailed(stderr))
        } else {
            Err(classify_ssh_error(&stderr, &self.host))
        }
    }
}
```

**`src/testutil.rs` — `MockSsh::download_file`:**

Add `download_responses: RefCell<VecDeque<Result<(), SkulkError>>>` to `MockSsh`. Add a builder `.with_download_responses(vec![...])`. Implement the method:
```rust
fn download_file(&self, remote_path: &str, local_path: &Path) -> Result<(), SkulkError> {
    self.calls
        .borrow_mut()
        .push(format!("DOWNLOAD {remote_path}:{}", local_path.display()));
    self.download_responses
        .borrow_mut()
        .pop_front()
        .unwrap_or(Ok(()))
}
```

## LocalOps Trait (re-use from skulk upload task)

`skulk download` needs a subset of the `LocalOps` trait introduced in the upload task:
- `git_status() -> Result<String, SkulkError>` — check for uncommitted changes
- `create_local_worktree(branch: &str, path: &Path) -> Result<(), SkulkError>` — runs `git worktree add <path> <branch>` locally; fetches the branch first if needed
- `claude_projects_dir() -> PathBuf` — `~/.claude/projects/`
- `list_remote_session_files(ssh: &impl Ssh, remote_dir: &str) -> Result<Vec<String>, SkulkError>` — run `ls ~/.claude/projects/<remote_dir>/` on remote and return filenames

If the upload task has already defined `LocalOps` in `src/commands/upload.rs`, add the new methods there. If not, define `LocalOps` in `src/commands/mod.rs` or a shared location.

Note: `create_local_worktree` requires git fetch before the worktree add if the remote branch isn't known locally. Command: `git fetch origin {branch_name} && git worktree add {local_path} {branch_name}`. This is a local git operation, not a remote SSH operation.

## Download Flow (step by step)

Implement `cmd_download` in `src/commands/download.rs`:

**Step 1: Validate agent name**
Call `validate_name(name)?`.

**Step 2: Check local git clean state**
Call `local.git_status()`. If non-empty, return error: `"Cannot download: local working tree has uncommitted changes. Commit or stash first."`

**Step 3: Verify remote agent exists**
Fetch remote inventory. The agent must have a worktree on the remote (`inv.worktrees.contains_key(&session_name)`). If not: `"Agent '{name}' not found on the remote."` Use `classify_agent_error` for consistency.

**Step 4: Compute local worktree path**
```rust
let agent = AgentRef::new(name, cfg);
let branch_name = agent.branch_name(); // "{session_prefix}{name}"
// Worktree goes at "../{branch_name}" relative to cwd
let local_worktree = std::env::current_dir()?.parent().unwrap().join(&branch_name);
```

**Step 5: Check local worktree path availability**
If `local_worktree.exists()` and `--force` is false: return error `"Cannot download: local path '../{branch_name}' already exists. Use --force to overwrite."`. With `--force`: remove the existing directory.

**Step 6: Check for existing local JSONL**
Compute the local Claude session dir for the future worktree path:
```rust
let encoded_local = claude_project_dir_name(&local_worktree.to_string_lossy());
let local_session_dir = local.claude_projects_dir().join(&encoded_local);
```
If `local_session_dir.exists()` and `--force` is false: return error `"Cannot download: local Claude session already exists at ~/.claude/projects/{encoded_local}/. Use --force to overwrite."`. With `--force`: clear the directory (or overwrite individual files).

**Step 7: Fetch remote JSONL file list**
Get the encoded remote path:
```rust
let worktree_path = agent.worktree_path(cfg);
let remote_encoded = ssh.run(&remote_claude_project_dir_command(&worktree_path))?;
let remote_session_dir = format!("~/.claude/projects/{remote_encoded}");
```
List files on remote: `ssh.run(&format!("ls {remote_session_dir} 2>/dev/null || true"))`. Parse the output into a list of filenames.

**Step 8: Create local git worktree**
Call `local.create_local_worktree(&branch_name, &local_worktree)?`.
This runs: `git fetch origin {branch_name} && git worktree add '{local_worktree}' {branch_name}`.
If fetch fails (branch not on origin), the agent worked locally and the branch might only be on the remote host. In that case the user should first run `skulk push {name}` to push the branch to origin, then retry download. Surface this as a helpful error message.

**Step 9: Copy JSONL files from remote to local**
Create the local session dir: `std::fs::create_dir_all(&local_session_dir)?`
For each filename from Step 7:
```rust
let remote_file = format!("{remote_session_dir}/{filename}");
let local_file = local_session_dir.join(&filename);
ssh.download_file(&remote_file, &local_file)?;
```

**Step 10: Auto-archive the remote agent**
Call `cmd_archive` (from `src/commands/interact.rs`) with the archive reason:
```rust
let hostname = std::env::var("HOSTNAME")
    .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
    .unwrap_or_else(|_| "unknown".to_string());
let reason = format!("downloaded to {hostname}");
cmd_archive(ssh, name, Some(&reason), cfg)?;
```

**Step 11: Print success message**
```
Downloaded agent '{name}' to {local_worktree}.
Agent '{name}' archived on {host}.
  Continue working: cd {local_worktree}
```

## New Files to Create

- `src/commands/download.rs` — the command implementation with co-located tests

## Files to Modify

- **`src/ssh.rs`**: Add `download_file` to the `Ssh` trait
- **`src/io.rs`**: Add `RealSsh::download_file` implementation
- **`src/testutil.rs`**: Add `download_responses` to `MockSsh` and implement `download_file`
- **`src/main.rs`**: Add `Download { name: String, force: bool }` to `Commands` enum, dispatch to `download::cmd_download`
- **`src/commands/mod.rs`**: Add `pub(crate) mod download;`

## Tests

Write unit tests in `src/commands/download.rs` using `MockSsh` and `MockLocalOps`. Key scenarios:

- `download_refuses_when_dirty_working_tree`: `git_status()` non-empty → error contains "uncommitted changes"
- `download_fails_when_agent_not_found`: inventory has no worktree for the name → `NotFound` error
- `download_refuses_when_local_path_exists_without_force`: local worktree path exists → error contains "already exists"
- `download_with_force_removes_existing_path`: `--force` set, local path exists → proceeds
- `download_copies_jsonl_files_from_remote`: happy path; verify `DOWNLOAD` calls appear in `ssh.calls()` for each remote JSONL file
- `download_archives_remote_agent_after_transfer`: verify archive SSH call (tmux kill) appears after download calls
- `download_archive_reason_contains_hostname`: verify the reason sidecar write includes "downloaded to"
- `download_skips_jsonl_when_no_remote_session`: remote `ls` returns empty → no `download_file` calls; proceeds to archive

Also add tests for `MockSsh::download_file` in `src/testutil.rs` (or `src/io.rs` tests):
- `mock_download_file_records_call`: verify the call is recorded as `"DOWNLOAD {remote}:{local}"`
- `mock_download_file_default_ok`: no responses queued → returns `Ok`

## Verification

```
cargo fmt
cargo clippy -- -D warnings -W clippy::pedantic
cargo test
```

All tests must pass. Check `cargo check` to ensure `MockSsh` compiles with the new `download_file` method.
