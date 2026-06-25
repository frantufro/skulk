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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, test_config};

    #[test]
    fn returns_err_when_ssh_succeeds() {
        // The stub always returns Err regardless of what the remote returns.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("-home-remote-worktrees-skulk-agent".into())]);
        let result = cmd_upload(&ssh, "agent", &cfg, "/home/local/skulk");
        assert!(result.is_err(), "upload stub must always return Err");
    }

    #[test]
    fn error_embeds_encoded_names() {
        // Verifies the remote_dir_cmd embeds the correct worktree path and that
        // the returned error message contains the expected encoded names.
        let cfg = test_config();
        // remote encoded name returned by the SSH command
        let remote_encoded = "-home-remote-worktrees-skulk-agent";
        let ssh = MockSsh::new(vec![Ok(remote_encoded.into())]);
        let result = cmd_upload(&ssh, "agent", &cfg, "/home/local/skulk");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("-home-local-skulk"),
            "error should contain local encoded path, got: {err_msg}"
        );
        assert!(
            err_msg.contains(remote_encoded),
            "error should contain remote encoded path, got: {err_msg}"
        );
    }
}
