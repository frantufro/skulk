use std::path::{Path, PathBuf};

use crate::error::SkulkError;

/// Local-side operations the `upload` and `download` commands need that are not
/// SSH calls: git queries against the local repo, local filesystem reads and
/// writes, local git worktree management, and temp-file handling for the upload
/// bundle.
///
/// Injected as a trait (with `MockLocalOps` in `testutil.rs` for the co-located
/// command tests, and the real `std::process::Command` / `std::fs`
/// implementation as `RealLocalOps` in `io.rs`) so the orchestration in
/// `cmd_upload` / `cmd_download` can be unit-tested without touching the real
/// git binary or filesystem.
pub(crate) trait LocalOps {
    // ── Shared ───────────────────────────────────────────────────────────────

    /// Run `git status --porcelain` in the project root. Empty output means the
    /// working tree is clean.
    fn git_status(&self) -> Result<String, SkulkError>;

    /// Return the path to the local `~/.claude/projects/` directory.
    fn claude_projects_dir(&self) -> PathBuf;

    // ── Upload ───────────────────────────────────────────────────────────────

    /// Run `git branch --show-current`. Returns the branch name (trimmed).
    fn git_current_branch(&self) -> Result<String, SkulkError>;

    /// Create a git bundle of `branch` at `dest`. Uses
    /// `git bundle create <dest> <branch>`.
    fn create_git_bundle(&self, branch: &str, dest: &Path) -> Result<(), SkulkError>;

    /// List all files (not directories) inside `dir`. Returns an empty vec if
    /// `dir` does not exist.
    fn list_dir_files(&self, dir: &Path) -> Result<Vec<PathBuf>, SkulkError>;

    /// Return the absolute path of the local project root (the directory
    /// containing `.skulk/`).
    fn project_root(&self) -> PathBuf;

    /// Return a temporary file path for the git bundle. `agent_name` keys the
    /// filename so concurrent uploads don't collide.
    fn temp_bundle_path(&self, agent_name: &str) -> PathBuf;

    /// Remove a local file (used to clean up the temp bundle after upload).
    fn remove_file(&self, path: &Path) -> Result<(), SkulkError>;

    // ── Download ─────────────────────────────────────────────────────────────

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
}
