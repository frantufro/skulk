use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::{claude_project_dir_name, remote_claude_project_dir_command};

/// Copy local Claude Code conversation JSONL files to the remote agent's
/// Claude project directory.
///
/// Encodes the local project path with [`claude_project_dir_name`] and
/// resolves the remote canonical path with [`remote_claude_project_dir_command`].
pub(crate) fn cmd_upload(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
    local_project_dir: &str,
) -> Result<(), SkulkError> {
    let local_encoded = claude_project_dir_name(local_project_dir);
    let worktree_path = format!("{}/{}{}", cfg.worktree_base, cfg.session_prefix, name);
    let remote_dir_cmd = remote_claude_project_dir_command(&worktree_path);
    let remote_encoded = ssh
        .run(&remote_dir_cmd)
        .map(|s| s.trim().to_owned())
        .map_err(|e| SkulkError::Validation(format!("failed to resolve remote path: {e}")))?;
    Err(SkulkError::Validation(format!(
        "skulk upload is not yet implemented (local={local_encoded}, remote={remote_encoded})"
    )))
}
