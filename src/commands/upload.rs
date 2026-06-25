use std::path::{Path, PathBuf};

use crate::agent_ref::AgentRef;
use crate::commands::new::{
    agent_create_tmux_command, agent_create_worktree_hooks_command,
    agent_rollback_worktree_command, format_rollback_failure_warning,
};
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::{
    branch_to_agent_name, claude_project_dir_name, remote_claude_project_dir_command, validate_name,
};

/// Local-side operations `skulk upload` needs that are not SSH calls: git
/// queries against the local repo, local filesystem reads, and temp-file
/// handling for the git bundle.
///
/// Injected as a trait (with `MockLocalOps` in the co-located tests, and the
/// real `std::process::Command` / `std::fs` implementation in `io.rs`) so the
/// orchestration in [`cmd_upload`] can be unit-tested without touching the real
/// git binary or filesystem. Mirrors the `run_local_command` injection used by
/// `init.rs`.
pub(crate) trait LocalOps {
    /// Run `git status --porcelain` in the project root. Returns stdout.
    fn git_status(&self) -> Result<String, SkulkError>;
    /// Run `git branch --show-current`. Returns the branch name (trimmed).
    fn git_current_branch(&self) -> Result<String, SkulkError>;
    /// Create a git bundle of `branch` at `dest`. Uses
    /// `git bundle create <dest> <branch>`.
    fn create_git_bundle(&self, branch: &str, dest: &Path) -> Result<(), SkulkError>;
    /// Return the path to the local `~/.claude/projects/` directory.
    fn claude_projects_dir(&self) -> PathBuf;
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
}

/// Transfer the current local branch and Claude Code conversation history to a
/// remote skulk agent so the agent can resume the work.
///
/// Two payloads move: the committed git state (via `git bundle` + the existing
/// `Ssh::upload_file` scp path, so no shared git remote is needed) and the
/// local Claude session JSONL files (copied into the agent's
/// `~/.claude/projects/<encoded>/` directory).
///
/// `to`: when `Some`, upload into an existing agent (must already have a
/// worktree); when `None`, create a new agent named after the local branch.
/// `force`: overwrite an existing remote Claude session in `--to` mode.
///
/// See the per-step comments below; the high-level flow is: validate local
/// state -> resolve agent name -> validate remote state -> bundle + transfer
/// git -> import branch + create worktree (new-agent mode) -> transfer Claude
/// session -> launch tmux session.
//
// Explicitly allow `too_many_lines`: `cmd_upload` is the linear orchestrator
// for the whole command and each block is a distinct, commented step. Splitting
// it would scatter the sequence without reducing complexity.
#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_upload(
    ssh: &impl Ssh,
    local: &dyn LocalOps,
    to: Option<&str>,
    force: bool,
    cfg: &Config,
) -> Result<(), SkulkError> {
    // Step 1: Local working tree must be clean.
    let status = local.git_status()?;
    if !status.trim().is_empty() {
        return Err(SkulkError::Validation(
            "Cannot upload: local working tree has uncommitted changes. Commit or stash first."
                .into(),
        ));
    }

    // Step 2: Determine the current branch (reject detached HEAD).
    let local_branch = local.git_current_branch()?;
    let local_branch = local_branch.trim();
    if local_branch.is_empty() {
        return Err(SkulkError::Validation(
            "Cannot upload: not on a named branch (detached HEAD). Check out a branch first."
                .into(),
        ));
    }

    // Step 3: Resolve the agent name. With `--to` it's explicit and validated
    // as typed; otherwise the branch name becomes the agent name, with `/`
    // namespacing flattened to `-` so namespaced branches (`feat/add-upload`)
    // are accepted while the remote worktree/state dirs stay flat.
    let agent_name = match to {
        Some(name) => name.to_string(),
        None => branch_to_agent_name(local_branch),
    };
    validate_name(&agent_name)?;

    let agent = AgentRef::new(&agent_name, cfg);
    let session_name = agent.session_name();
    let branch_name = agent.branch_name();
    let worktree_path = agent.worktree_path(cfg);
    let host = &cfg.host;

    // Step 4: Validate remote state in a single inventory round-trip.
    let inv = fetch_inventory(ssh, cfg)?;
    let new_agent_mode = to.is_none();
    if new_agent_mode {
        // Same uniqueness rules as `skulk new`.
        if inv.sessions.contains(&session_name) {
            return Err(SkulkError::Validation(format!(
                "Agent '{agent_name}' already exists."
            )));
        }
        if inv.worktrees.contains_key(&session_name) {
            return Err(SkulkError::Validation(format!(
                "Agent '{agent_name}' already has a worktree (archived or crashed).\n  \
                 Resume it: `skulk restart {agent_name}`\n  \
                 Or wipe it: `skulk destroy {agent_name}`"
            )));
        }
    } else if !inv.worktrees.contains_key(&session_name) {
        return Err(SkulkError::Validation(format!(
            "Agent '{agent_name}' has no worktree on the remote. \
             Run `skulk new {agent_name}` first."
        )));
    }

    // Step 7a (--to mode): refuse to clobber an existing remote Claude session
    // unless --force was given. Built from the shared path-encoding helper so
    // the probe and Step 10 encode the worktree path identically.
    if !new_agent_mode && !force {
        let probe = format!(
            "test -d ~/.claude/projects/$({}) && echo exists",
            remote_claude_project_dir_command(&worktree_path)
        );
        if let Ok(out) = ssh.run(&probe)
            && out.trim() == "exists"
        {
            return Err(SkulkError::Validation(format!(
                "Agent '{agent_name}' already has a Claude session on the remote. \
                 Use --force to overwrite."
            )));
        }
    }

    // Steps 5/6/7b/7c/8 (new-agent mode only): the git bundle exists solely to
    // import the branch and seed the worktree. In --to mode the agent already
    // has a worktree, so the bundle would be uploaded then deleted unread —
    // skip it entirely and only transfer the Claude session below.
    if new_agent_mode {
        // Step 5: Create the git bundle locally.
        let bundle_path = local.temp_bundle_path(&agent_name);
        local.create_git_bundle(local_branch, &bundle_path)?;

        // Step 6: Transfer the bundle to the remote. Preserve the classified
        // SSH error variant rather than flattening it into Validation.
        let remote_bundle = format!("/tmp/skulk-upload-{session_name}.bundle");
        if let Err(e) = ssh.upload_file(&bundle_path, &remote_bundle) {
            cleanup_local_bundle(local, &bundle_path);
            return Err(classify_agent_error(&agent_name, e, host));
        }

        // Step 7b: import the branch from the bundle. On failure, clean up both
        // the remote and local temp bundles before returning.
        let base_path = &cfg.base_path;
        if let Err(e) = ssh.run(&format!(
            "cd {base_path} && git fetch {remote_bundle} {local_branch}:{branch_name}"
        )) {
            let _ = ssh.run(&format!("rm -f {remote_bundle}"));
            cleanup_local_bundle(local, &bundle_path);
            return Err(e);
        }

        // Step 7c: create the worktree pointed at the imported branch, with the
        // harness hooks installed. On failure, roll back the freshly-imported
        // branch and clean up both temp bundles.
        if let Err(e) = ssh.run(&agent_create_worktree_hooks_command(&agent_name, cfg)) {
            if ssh
                .run(&agent_rollback_worktree_command(&agent_name, cfg))
                .is_err()
            {
                eprintln!("{}", format_rollback_failure_warning(&agent_name));
            }
            let _ = ssh.run(&format!("rm -f {remote_bundle}"));
            cleanup_local_bundle(local, &bundle_path);
            return Err(e);
        }

        // Step 8: Clean up the remote bundle (non-fatal).
        let _ = ssh.run(&format!("rm -f {remote_bundle}"));

        // Step 9: Clean up the local bundle (non-fatal).
        cleanup_local_bundle(local, &bundle_path);
    }

    // Step 10: Transfer the local Claude session files. Skip silently when the
    // local project has no session history — the agent just starts fresh.
    let local_root = local.project_root();
    let encoded_local = claude_project_dir_name(&local_root.to_string_lossy());
    let local_session_dir = local.claude_projects_dir().join(&encoded_local);
    let files = local.list_dir_files(&local_session_dir)?;
    if !files.is_empty() {
        let remote_encoded = ssh.run(&remote_claude_project_dir_command(&worktree_path))?;
        let remote_encoded = remote_encoded.trim();
        let remote_session_dir = format!("~/.claude/projects/{remote_encoded}");
        ssh.run(&format!("mkdir -p {remote_session_dir}"))?;
        for file in files {
            let Some(filename) = file.file_name() else {
                continue;
            };
            let remote_path = format!("{remote_session_dir}/{}", filename.to_string_lossy());
            if let Err(e) = ssh.upload_file(&file, &remote_path) {
                return Err(classify_agent_error(&agent_name, e, host));
            }
        }
    }

    // Step 11: Launch a tmux session running the harness in the worktree. In
    // new-agent mode a tmux failure would strand the freshly-created worktree
    // and imported branch, so best-effort roll them back (mirroring `cmd_new`).
    if let Err(e) = ssh.run(&agent_create_tmux_command(
        &agent_name,
        cfg,
        false,
        None,
        None,
    )) {
        if new_agent_mode
            && ssh
                .run(&agent_rollback_worktree_command(&agent_name, cfg))
                .is_err()
        {
            eprintln!("{}", format_rollback_failure_warning(&agent_name));
        }
        return Err(e);
    }

    // Step 12: Success.
    println!(
        "Uploaded '{local_branch}' to agent '{agent_name}' on {host}.\n  \
         Connect: skulk connect {agent_name}\n  \
         Watch:   skulk logs {agent_name} --follow"
    );

    Ok(())
}

/// Remove the local temp bundle, warning (but not failing) on error. The bundle
/// is a disposable temp file; a leaked one is harmless, so cleanup never aborts
/// an otherwise-successful upload.
fn cleanup_local_bundle(local: &dyn LocalOps, bundle_path: &Path) {
    if let Err(e) = local.remove_file(bundle_path) {
        eprintln!(
            "Warning: failed to remove temp bundle {}: {e}",
            bundle_path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockLocalOps, MockSsh, mock_empty_inventory, mock_inventory, ssh_ok, test_config,
    };

    #[test]
    fn upload_refuses_when_dirty_working_tree() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let mut local = MockLocalOps::clean();
        local.status = Ok(" M src/main.rs\n".into());

        let result = cmd_upload(&ssh, &local, None, false, &cfg);

        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("uncommitted changes"),
            "expected dirty-tree error, got: {msg}"
        );
        assert!(ssh.calls().is_empty(), "must not touch SSH on dirty tree");
    }

    #[test]
    fn upload_refuses_on_detached_head() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let mut local = MockLocalOps::clean();
        local.branch = Ok(String::new());

        let result = cmd_upload(&ssh, &local, None, false, &cfg);

        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("detached HEAD"),
            "expected detached-HEAD error, got: {msg}"
        );
    }

    #[test]
    fn upload_creates_new_agent_from_branch() {
        let cfg = test_config();
        // Inventory (empty), git fetch, worktree+hooks, rm bundle, tmux create.
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            ssh_ok(), // git fetch
            ssh_ok(), // worktree + hooks
            ssh_ok(), // rm remote bundle
            ssh_ok(), // tmux create
        ]);
        let local = MockLocalOps::clean();

        let result = cmd_upload(&ssh, &local, None, false, &cfg);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("git fetch") && c.contains("feature:skulk-feature")),
            "expected git fetch importing the branch, got: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("git worktree add")),
            "expected worktree creation, got: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("tmux new-session")),
            "expected tmux create, got: {calls:?}"
        );
        // Bundle is uploaded via scp (recorded as UPLOAD ...).
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("UPLOAD") && c.contains("skulk-feature.bundle")),
            "expected bundle upload, got: {calls:?}"
        );
    }

    #[test]
    fn upload_new_agent_uploads_session_files_and_makes_remote_dir() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            ssh_ok(),                                          // git fetch
            ssh_ok(),                                          // worktree + hooks
            ssh_ok(),                                          // rm remote bundle
            Ok("-home-remote-worktrees-skulk-feature".into()), // remote project dir
            ssh_ok(),                                          // mkdir remote session dir
            ssh_ok(),                                          // tmux create
        ]);
        let mut local = MockLocalOps::clean();
        local.files = Ok(vec![PathBuf::from(
            "/home/local/.claude/projects/-home-local-skulk/session-abc.jsonl",
        )]);

        let result = cmd_upload(&ssh, &local, None, false, &cfg);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("mkdir -p ~/.claude/projects/")),
            "expected mkdir for remote session dir, got: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("UPLOAD") && c.contains("session-abc.jsonl")),
            "expected JSONL upload, got: {calls:?}"
        );
    }

    #[test]
    fn upload_skips_session_transfer_when_no_local_history() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            ssh_ok(), // git fetch
            ssh_ok(), // worktree + hooks
            ssh_ok(), // rm remote bundle
            ssh_ok(), // tmux create
        ]);
        let local = MockLocalOps::clean(); // files = empty

        let result = cmd_upload(&ssh, &local, None, false, &cfg);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let calls = ssh.calls();
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("mkdir -p ~/.claude/projects/")),
            "must not mkdir session dir with no history, got: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains(".jsonl")),
            "must not upload any JSONL with no history, got: {calls:?}"
        );
    }

    #[test]
    fn upload_to_existing_refuses_without_force_when_session_exists() {
        let cfg = test_config();
        // Inventory shows the worktree present; the test -d probe returns "exists".
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-target", "/wt/skulk-target")],
                &["skulk-target"],
            )),
            Ok("exists".into()), // remote claude session probe
            ssh_ok(),            // rm remote bundle
        ]);
        let local = MockLocalOps::clean();

        let result = cmd_upload(&ssh, &local, Some("target"), false, &cfg);

        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("already has a Claude session"),
            "expected existing-session error, got: {msg}"
        );
    }

    #[test]
    fn upload_to_existing_with_force_overwrites() {
        let cfg = test_config();
        // With --force, the session probe is skipped; flow proceeds to session
        // transfer + tmux create. No git fetch / worktree / bundle in --to mode.
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-target", "/wt/skulk-target")],
                &["skulk-target"],
            )),
            ssh_ok(), // tmux create
        ]);
        let local = MockLocalOps::clean(); // no session files

        let result = cmd_upload(&ssh, &local, Some("target"), true, &cfg);
        assert!(result.is_ok(), "expected Ok with --force, got: {result:?}");

        let calls = ssh.calls();
        assert!(
            !calls.iter().any(|c| c.contains("git fetch")),
            "--to mode must not import a branch, got: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("git worktree add")),
            "--to mode must not create a worktree, got: {calls:?}"
        );
        // --to mode never builds or ships the git bundle: nothing to upload
        // and no /tmp bundle to remove.
        assert!(
            !calls.iter().any(|c| c.contains(".bundle")),
            "--to mode must not touch a git bundle, got: {calls:?}"
        );
    }

    #[test]
    fn upload_to_existing_with_force_transfers_session_files() {
        let cfg = test_config();
        // --to + force, with local session history: the JSONL must land in the
        // existing worktree's remote session dir.
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-target", "/wt/skulk-target")],
                &["skulk-target"],
            )),
            Ok("-wt-skulk-target".into()), // remote project dir
            ssh_ok(),                      // mkdir remote session dir
            ssh_ok(),                      // tmux create
        ]);
        let mut local = MockLocalOps::clean();
        local.files = Ok(vec![PathBuf::from(
            "/home/local/.claude/projects/-home-local-skulk/session-xyz.jsonl",
        )]);

        let result = cmd_upload(&ssh, &local, Some("target"), true, &cfg);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("mkdir -p ~/.claude/projects/")),
            "expected mkdir for existing worktree's session dir, got: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("UPLOAD") && c.contains("session-xyz.jsonl")),
            "expected JSONL upload into existing worktree, got: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains(".bundle")),
            "--to mode must not touch a git bundle, got: {calls:?}"
        );
    }

    #[test]
    fn upload_rolls_back_worktree_on_tmux_failure() {
        let cfg = test_config();
        // New-agent mode: tmux create fails after the worktree + branch are
        // imported. The worktree must be rolled back so gc isn't left an orphan.
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            ssh_ok(),                                        // git fetch
            ssh_ok(),                                        // worktree + hooks
            ssh_ok(),                                        // rm remote bundle
            Err(SkulkError::SshFailed("tmux: boom".into())), // tmux create FAILS
            ssh_ok(),                                        // rollback worktree
        ]);
        let local = MockLocalOps::clean();

        let result = cmd_upload(&ssh, &local, None, false, &cfg);
        assert!(result.is_err(), "expected error on tmux failure");

        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("git worktree remove") && c.contains("git branch -D")),
            "expected rollback (worktree remove + branch -D), got: {calls:?}"
        );
    }

    #[test]
    fn upload_to_existing_refuses_when_no_worktree() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let local = MockLocalOps::clean();

        let result = cmd_upload(&ssh, &local, Some("ghost"), false, &cfg);

        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("no worktree"),
            "expected missing-worktree error, got: {msg}"
        );
    }

    #[test]
    fn upload_new_agent_refuses_duplicate() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(
            &["skulk-feature"],
            &[("skulk-feature", "/wt/skulk-feature")],
            &["skulk-feature"],
        ))]);
        let local = MockLocalOps::clean();

        let result = cmd_upload(&ssh, &local, None, false, &cfg);

        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("already exists"),
            "expected duplicate-agent error, got: {msg}"
        );
    }

    #[test]
    fn upload_bundle_cleanup_is_nonfatal() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            ssh_ok(), // git fetch
            ssh_ok(), // worktree + hooks
            ssh_ok(), // rm remote bundle
            ssh_ok(), // tmux create
        ]);
        let mut local = MockLocalOps::clean();
        local.remove = Err(SkulkError::SshFailed("permission denied".into()));

        let result = cmd_upload(&ssh, &local, None, false, &cfg);
        assert!(
            result.is_ok(),
            "bundle cleanup failure must not abort upload, got: {result:?}"
        );
        assert!(
            !local.removed.borrow().is_empty(),
            "remove_file should have been attempted"
        );
    }

    #[test]
    fn upload_sanitizes_slash_in_branch_to_agent_name() {
        // A namespaced branch (feat/add-thing) is flattened to a valid agent
        // name (feat-add-thing) so the remote worktree/state dirs stay flat,
        // and the import targets the sanitized branch ref.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            ssh_ok(), // git fetch
            ssh_ok(), // worktree + hooks
            ssh_ok(), // rm remote bundle
            ssh_ok(), // tmux create
        ]);
        let mut local = MockLocalOps::clean();
        local.branch = Ok("feat/add-thing".into());

        let result = cmd_upload(&ssh, &local, None, false, &cfg);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let calls = ssh.calls();
        // The bundle still carries the original local branch, imported as the
        // sanitized remote branch ref `skulk-feat-add-thing`.
        assert!(
            calls
                .iter()
                .any(|c| c.contains("git fetch")
                    && c.contains("feat/add-thing:skulk-feat-add-thing")),
            "expected fetch importing sanitized branch, got: {calls:?}"
        );
    }

    #[test]
    fn upload_rejects_branch_with_unsanitizable_char() {
        // A branch with a character that slash-flattening can't fix (a space)
        // is still rejected by validate_name before any SSH call.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let mut local = MockLocalOps::clean();
        local.branch = Ok("feat add thing".into());

        let result = cmd_upload(&ssh, &local, None, false, &cfg);

        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Invalid character"),
            "expected name-validation error, got: {msg}"
        );
        assert!(ssh.calls().is_empty(), "must not touch SSH on invalid name");
    }
}
