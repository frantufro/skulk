use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::agent_ref::AgentRef;
use crate::commands::interact::cmd_archive;
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::{
    claude_project_dir_name, remote_claude_project_dir_command, shell_escape, validate_name,
};

/// Local filesystem and git operations needed by `skulk download`.
///
/// Split into a trait so `cmd_download` can be unit-tested with a fully
/// in-memory mock — none of these touch a real git repo, the user's home
/// directory, or the working tree during tests.
pub(crate) trait LocalOps {
    /// `git status --porcelain` in the current directory. Empty output means
    /// the working tree is clean.
    fn git_status(&self) -> Result<String, SkulkError>;

    /// The current working directory.
    fn current_dir(&self) -> Result<PathBuf, SkulkError>;

    /// Whether a path exists on the local filesystem.
    fn path_exists(&self, path: &Path) -> bool;

    /// Recursively remove a directory (used by `--force` to clear stale paths).
    fn remove_dir_all(&self, path: &Path) -> Result<(), SkulkError>;

    /// Recursively create a directory.
    fn create_dir_all(&self, path: &Path) -> Result<(), SkulkError>;

    /// Create a local git worktree for `branch` at `path`.
    ///
    /// Fetches the branch from `origin` first so a branch that only exists on
    /// the remote host (never pushed) surfaces a helpful error rather than a
    /// bare `git worktree add` failure.
    fn create_local_worktree(&self, branch: &str, path: &Path) -> Result<(), SkulkError>;

    /// Remove a previously-created local git worktree at `path`.
    ///
    /// Used to roll back after a mid-transfer failure. Best-effort: removal
    /// failures are surfaced to the caller, which logs them rather than
    /// masking the original error.
    fn remove_local_worktree(&self, path: &Path);

    /// The local Claude Code projects directory (`~/.claude/projects`).
    fn claude_projects_dir(&self) -> PathBuf;
}

/// Production [`LocalOps`] backed by the real filesystem and `git`.
pub(crate) struct RealLocalOps;

impl RealLocalOps {
    /// Run a local `git` invocation, returning trimmed stdout on success or a
    /// `SkulkError::SshFailed` carrying stderr on failure.
    fn run_git(args: &[&str]) -> Result<String, SkulkError> {
        let output = ProcessCommand::new("git")
            .args(args)
            .output()
            .map_err(|e| SkulkError::SshExec(format!("failed to run git: {e}")))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(SkulkError::SshFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
}

impl LocalOps for RealLocalOps {
    fn git_status(&self) -> Result<String, SkulkError> {
        Self::run_git(&["status", "--porcelain"])
    }

    fn current_dir(&self) -> Result<PathBuf, SkulkError> {
        std::env::current_dir()
            .map_err(|e| SkulkError::Validation(format!("Cannot determine current directory: {e}")))
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), SkulkError> {
        std::fs::remove_dir_all(path).map_err(|e| {
            SkulkError::Validation(format!("Failed to remove {}: {e}", path.display()))
        })
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), SkulkError> {
        std::fs::create_dir_all(path).map_err(|e| {
            SkulkError::Validation(format!("Failed to create {}: {e}", path.display()))
        })
    }

    fn create_local_worktree(&self, branch: &str, path: &Path) -> Result<(), SkulkError> {
        // Fetch the branch from origin first. If the branch was never pushed,
        // this fails and we surface a guiding error pointing at `skulk push`.
        if Self::run_git(&["fetch", "origin", branch]).is_err() {
            return Err(SkulkError::Diagnostic {
                message: format!("Branch '{branch}' is not on origin."),
                suggestion: format!(
                    "Run `skulk push {branch}` (or push the branch to origin) and retry."
                ),
            });
        }
        // Prune stale per-worktree metadata under `.git/worktrees/<name>`.
        // A `--force` re-download removes the worktree directory from the
        // filesystem but leaves git's bookkeeping behind; without pruning,
        // `git worktree add` below fails with "already registered".
        Self::run_git(&["worktree", "prune"])?;
        let path_str = path.to_string_lossy();
        Self::run_git(&["worktree", "add", &path_str, branch]).map(|_| ())
    }

    fn remove_local_worktree(&self, path: &Path) {
        // `git worktree remove --force` clears both the directory and git's
        // per-worktree metadata, leaving no stale registration behind.
        let path_str = path.to_string_lossy();
        if let Err(e) = Self::run_git(&["worktree", "remove", "--force", &path_str]) {
            eprintln!(
                "Warning: failed to roll back worktree {}: {e}",
                path.display()
            );
        }
    }

    fn claude_projects_dir(&self) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".claude").join("projects")
    }
}

/// Resolve the local machine's hostname for the auto-archive reason annotation.
fn local_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Build the SSH command that records why an agent was archived.
///
/// Stored at `~/.skulk/archive/<session_name>.txt`. `reason` is the raw,
/// unescaped string; this function applies `shell_escape` internally.
///
/// NOTE: this sidecar is currently forward-looking and intentionally unread —
/// no command consumes it yet. It is written so that, once `cmd_archive` grows
/// an optional `reason` argument (dependency task
/// `add-optional-reason-flag-to-skulk-archive`), `skulk list`/`status` can
/// surface the download origin. Until then it is a best-effort annotation.
fn archive_reason_command(session_name: &str, reason: &str) -> String {
    format!(
        "mkdir -p ~/.skulk/archive && printf '%s' '{}' > ~/.skulk/archive/{session_name}.txt",
        shell_escape(reason)
    )
}

/// Bring a remote agent's branch and Claude session to a local worktree.
///
/// Reverse of `skulk upload`: transfers the agent's git branch into a new local
/// worktree at `../<branch-name>` and copies its Claude Code conversation files
/// into the matching `~/.claude/projects/` directory, then auto-archives the
/// remote agent (tmux session killed, worktree and branch preserved).
pub(crate) fn cmd_download(
    ssh: &impl Ssh,
    local: &impl LocalOps,
    name: &str,
    force: bool,
    cfg: &Config,
) -> Result<(), SkulkError> {
    // Step 1: Validate agent name.
    validate_name(name)?;

    // Step 2: Check local git clean state.
    let status = local.git_status()?;
    if !status.trim().is_empty() {
        return Err(SkulkError::Validation(
            "Cannot download: local working tree has uncommitted changes. Commit or stash first."
                .into(),
        ));
    }

    // Step 3: Verify remote agent exists.
    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let inv = fetch_inventory(ssh, cfg).map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    if !inv.worktrees.contains_key(&session_name) {
        return Err(SkulkError::NotFound(format!(
            "Agent '{name}' not found on the remote."
        )));
    }

    // Step 4: Compute local worktree path (`../<branch-name>`).
    let branch_name = agent.branch_name();
    let cwd = local.current_dir()?;
    let parent = cwd.parent().ok_or_else(|| {
        SkulkError::Validation(
            "Cannot download: current directory has no parent to host the worktree.".into(),
        )
    })?;
    let local_worktree = parent.join(&branch_name);

    // Step 5: Check local worktree path availability.
    if local.path_exists(&local_worktree) {
        if force {
            local.remove_dir_all(&local_worktree)?;
        } else {
            return Err(SkulkError::Validation(format!(
                "Cannot download: local path '../{branch_name}' already exists. Use --force to overwrite."
            )));
        }
    }

    // Step 6: Check for existing local Claude session directory.
    let encoded_local = claude_project_dir_name(&local_worktree.to_string_lossy());
    let local_session_dir = local.claude_projects_dir().join(&encoded_local);
    if local.path_exists(&local_session_dir) {
        if force {
            local.remove_dir_all(&local_session_dir)?;
        } else {
            return Err(SkulkError::Validation(format!(
                "Cannot download: local Claude session already exists at ~/.claude/projects/{encoded_local}/. Use --force to overwrite."
            )));
        }
    }

    // Step 7: Fetch the remote JSONL file list.
    let worktree_path = agent.worktree_path(cfg);
    let remote_encoded = ssh
        .run(&remote_claude_project_dir_command(&worktree_path))
        .map(|s| s.trim().to_owned())
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    let remote_session_dir = format!("~/.claude/projects/{remote_encoded}");
    let listing = ssh
        .run(&format!("ls {remote_session_dir} 2>/dev/null || true"))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    let filenames: Vec<String> = listing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect();

    // Step 8: Create the local git worktree.
    local.create_local_worktree(&branch_name, &local_worktree)?;

    // Step 9: Copy JSONL files from remote to local. The transfer must be
    // atomic: if any file fails to download we roll back the just-created
    // worktree and the partially populated session directory so a retry sees
    // a clean slate rather than tripping the Step 5 "path already exists"
    // guard (which would then require --force).
    if !filenames.is_empty() {
        local.create_dir_all(&local_session_dir)?;
        for filename in &filenames {
            let remote_file = format!("{remote_session_dir}/{filename}");
            let local_file = local_session_dir.join(filename);
            if let Err(e) = ssh.download_file(&remote_file, &local_file) {
                local.remove_local_worktree(&local_worktree);
                let _ = local.remove_dir_all(&local_session_dir);
                return Err(classify_agent_error(name, e, &cfg.host));
            }
        }
    }

    // Step 10: Auto-archive the remote agent, recording the download origin.
    // The transfer has already succeeded, so archive failures (e.g. the remote
    // tmux session is already gone) are best-effort: warn and continue rather
    // than reporting overall failure for an agent that is fully downloaded.
    if let Err(e) = cmd_archive(ssh, name, cfg) {
        eprintln!("Warning: failed to archive remote agent '{name}': {e}");
    }
    let reason = format!("downloaded to {}", local_hostname());
    if let Err(e) = ssh.run(&archive_reason_command(&session_name, &reason)) {
        eprintln!("Warning: failed to record archive reason for '{name}': {e}");
    }

    // Step 11: Print success message.
    let worktree_display = local_worktree.display();
    eprintln!("Downloaded agent '{name}' to {worktree_display}.");
    eprintln!("Agent '{name}' archived on {}.", cfg.host);
    eprintln!("  Continue working: cd {worktree_display}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use super::*;
    use crate::testutil::{MockSsh, mock_empty_inventory, mock_inventory, ssh_ok, test_config};

    /// In-memory [`LocalOps`] for deterministic `cmd_download` tests.
    struct MockLocalOps {
        git_status: String,
        cwd: PathBuf,
        existing_paths: HashSet<PathBuf>,
        claude_projects: PathBuf,
        worktree_result: RefCell<Option<Result<(), SkulkError>>>,
        removed: RefCell<Vec<PathBuf>>,
        created_worktrees: RefCell<Vec<(String, PathBuf)>>,
        removed_worktrees: RefCell<Vec<PathBuf>>,
    }

    impl MockLocalOps {
        fn clean() -> Self {
            Self {
                git_status: String::new(),
                cwd: PathBuf::from("/home/user/project"),
                existing_paths: HashSet::new(),
                claude_projects: PathBuf::from("/home/user/.claude/projects"),
                worktree_result: RefCell::new(None),
                removed: RefCell::new(Vec::new()),
                created_worktrees: RefCell::new(Vec::new()),
                removed_worktrees: RefCell::new(Vec::new()),
            }
        }

        fn with_dirty(mut self, status: &str) -> Self {
            self.git_status = status.to_string();
            self
        }

        fn with_existing(mut self, path: &str) -> Self {
            self.existing_paths.insert(PathBuf::from(path));
            self
        }

        fn with_worktree_err(self, err: SkulkError) -> Self {
            *self.worktree_result.borrow_mut() = Some(Err(err));
            self
        }
    }

    impl LocalOps for MockLocalOps {
        fn git_status(&self) -> Result<String, SkulkError> {
            Ok(self.git_status.clone())
        }

        fn current_dir(&self) -> Result<PathBuf, SkulkError> {
            Ok(self.cwd.clone())
        }

        fn path_exists(&self, path: &Path) -> bool {
            self.existing_paths.contains(path)
        }

        fn remove_dir_all(&self, path: &Path) -> Result<(), SkulkError> {
            self.removed.borrow_mut().push(path.to_path_buf());
            Ok(())
        }

        fn create_dir_all(&self, _path: &Path) -> Result<(), SkulkError> {
            Ok(())
        }

        fn create_local_worktree(&self, branch: &str, path: &Path) -> Result<(), SkulkError> {
            self.created_worktrees
                .borrow_mut()
                .push((branch.to_string(), path.to_path_buf()));
            self.worktree_result.borrow_mut().take().unwrap_or(Ok(()))
        }

        fn remove_local_worktree(&self, path: &Path) {
            self.removed_worktrees.borrow_mut().push(path.to_path_buf());
        }

        fn claude_projects_dir(&self) -> PathBuf {
            self.claude_projects.clone()
        }
    }

    /// Inventory in which `name` (session-prefixed) has a worktree on the remote.
    fn inventory_with_agent(session_name: &str) -> String {
        let path = format!("/remote/worktrees/{session_name}");
        mock_inventory(&[session_name], &[(session_name, &path)], &[session_name])
    }

    #[test]
    fn download_refuses_when_dirty_working_tree() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let local = MockLocalOps::clean().with_dirty(" M src/main.rs");
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(
            matches!(&result, Err(SkulkError::Validation(msg)) if msg.contains("uncommitted changes"))
        );
    }

    #[test]
    fn download_fails_when_agent_not_found() {
        let cfg = test_config();
        // Inventory has no worktree for the requested name.
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let local = MockLocalOps::clean();
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(matches!(&result, Err(SkulkError::NotFound(msg)) if msg.contains("task")));
    }

    #[test]
    fn download_refuses_when_local_path_exists_without_force() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(inventory_with_agent("skulk-task"))]);
        let local = MockLocalOps::clean().with_existing("/home/user/skulk-task");
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(
            matches!(&result, Err(SkulkError::Validation(msg)) if msg.contains("already exists"))
        );
    }

    #[test]
    fn download_refuses_when_local_claude_session_exists_without_force() {
        let cfg = test_config();
        // Inventory + remote-encoded-path lookup happen before Step 6's guard.
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
        ]);
        // The encoded local session dir for the computed worktree path exists.
        let encoded = claude_project_dir_name("/home/user/skulk-task");
        let session_dir = format!("/home/user/.claude/projects/{encoded}");
        let local = MockLocalOps::clean().with_existing(&session_dir);
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(
            matches!(&result, Err(SkulkError::Validation(msg)) if msg.contains("Claude session already exists")),
            "expected Claude-session-exists Validation error, got {result:?}"
        );
    }

    #[test]
    fn download_with_force_removes_existing_claude_session() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            ssh_ok(), // ls listing (empty -> no files)
            ssh_ok(), // archive kill
            ssh_ok(), // archive reason
        ]);
        let encoded = claude_project_dir_name("/home/user/skulk-task");
        let session_dir = PathBuf::from(format!("/home/user/.claude/projects/{encoded}"));
        let local = MockLocalOps::clean().with_existing(&session_dir.to_string_lossy());
        let result = cmd_download(&ssh, &local, "task", true, &cfg);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(
            local.removed.borrow().contains(&session_dir),
            "force should remove the existing Claude session dir: {:?}",
            local.removed.borrow()
        );
    }

    #[test]
    fn download_rolls_back_worktree_and_session_on_transfer_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            Ok("a.jsonl".into()), // one file to transfer
        ])
        .with_download_responses(vec![Err(SkulkError::SshFailed("scp failed".into()))]);
        let local = MockLocalOps::clean();
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(
            matches!(&result, Err(SkulkError::SshFailed(_))),
            "transfer failure should propagate, got {result:?}"
        );
        // The just-created worktree is rolled back...
        assert!(
            local
                .removed_worktrees
                .borrow()
                .contains(&PathBuf::from("/home/user/skulk-task")),
            "worktree should be rolled back on transfer failure"
        );
        // ...and the partial session dir is removed.
        let encoded = claude_project_dir_name("/home/user/skulk-task");
        let session_dir = PathBuf::from(format!("/home/user/.claude/projects/{encoded}"));
        assert!(
            local.removed.borrow().contains(&session_dir),
            "partial session dir should be removed on transfer failure"
        );
        // The remote agent must NOT be archived after a failed transfer.
        assert!(
            !ssh.calls().iter().any(|c| c.contains("kill-session")),
            "remote agent must not be archived after a failed transfer"
        );
    }

    #[test]
    fn download_succeeds_when_remote_archive_fails() {
        let cfg = test_config();
        // Transfer succeeds; the archive kill-session returns an error
        // (e.g. the tmux session is already gone). Download must still succeed.
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            Ok("a.jsonl".into()),
            Err(SkulkError::SshFailed("no server running".into())), // archive kill fails
            ssh_ok(),                                               // archive reason
        ]);
        let local = MockLocalOps::clean();
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(
            result.is_ok(),
            "archive failure must be best-effort, got {result:?}"
        );
    }

    #[test]
    fn download_with_force_removes_existing_path() {
        let cfg = test_config();
        // inventory, remote-encoded-path, ls listing, archive kill, archive reason
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        let local = MockLocalOps::clean().with_existing("/home/user/skulk-task");
        let result = cmd_download(&ssh, &local, "task", true, &cfg);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(
            local
                .removed
                .borrow()
                .contains(&PathBuf::from("/home/user/skulk-task")),
            "force should remove the existing worktree path"
        );
    }

    #[test]
    fn download_copies_jsonl_files_from_remote() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            Ok("a.jsonl\nb.jsonl".into()),
            ssh_ok(), // archive kill
            ssh_ok(), // archive reason
        ]);
        let local = MockLocalOps::clean();
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let downloads: Vec<String> = ssh
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("DOWNLOAD "))
            .collect();
        assert_eq!(downloads.len(), 2, "expected one DOWNLOAD per jsonl file");
        assert!(downloads[0].contains("a.jsonl"));
        assert!(downloads[1].contains("b.jsonl"));
    }

    #[test]
    fn download_archives_remote_agent_after_transfer() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            Ok("a.jsonl".into()),
            ssh_ok(), // archive kill
            ssh_ok(), // archive reason
        ]);
        let local = MockLocalOps::clean();
        cmd_download(&ssh, &local, "task", false, &cfg).expect("download should succeed");
        let calls = ssh.calls();
        let download_idx = calls
            .iter()
            .position(|c| c.starts_with("DOWNLOAD "))
            .expect("expected a DOWNLOAD call");
        let kill_idx = calls
            .iter()
            .position(|c| c.contains("kill-session"))
            .expect("expected an archive kill-session call");
        assert!(
            download_idx < kill_idx,
            "archive must happen after file transfer: {calls:?}"
        );
    }

    #[test]
    fn download_archive_reason_contains_hostname() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            Ok(String::new()), // empty listing
            ssh_ok(),          // archive kill
            ssh_ok(),          // archive reason
        ]);
        let local = MockLocalOps::clean();
        cmd_download(&ssh, &local, "task", false, &cfg).expect("download should succeed");
        let calls = ssh.calls();
        let reason_call = calls
            .iter()
            .find(|c| c.contains("skulk/archive"))
            .expect("expected an archive reason sidecar write");
        assert!(
            reason_call.contains("downloaded to"),
            "reason should record the download origin: {reason_call}"
        );
    }

    #[test]
    fn download_skips_jsonl_when_no_remote_session() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            Ok(String::new()), // empty listing — no session files
            ssh_ok(),          // archive kill
            ssh_ok(),          // archive reason
        ]);
        let local = MockLocalOps::clean();
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let downloads = ssh
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("DOWNLOAD "))
            .count();
        assert_eq!(downloads, 0, "no files should be downloaded");
        // It still creates the worktree and archives.
        assert_eq!(local.created_worktrees.borrow().len(), 1);
    }

    #[test]
    fn download_refuses_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let local = MockLocalOps::clean();
        let result = cmd_download(&ssh, &local, "../bad", false, &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn download_surfaces_unpushed_branch_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(inventory_with_agent("skulk-task")),
            Ok("-remote-worktrees-skulk-task".into()),
            Ok("a.jsonl".into()),
        ]);
        let local = MockLocalOps::clean().with_worktree_err(SkulkError::Diagnostic {
            message: "Branch 'skulk-task' is not on origin.".into(),
            suggestion: "Run `skulk push task`.".into(),
        });
        let result = cmd_download(&ssh, &local, "task", false, &cfg);
        assert!(matches!(result, Err(SkulkError::Diagnostic { .. })));
    }
}
