use crate::agent_ref::AgentRef;
use crate::commands::new::agent_create_tmux_command;
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Restart an agent in its existing worktree with a fresh Claude session.
///
/// Use after `skulk archive`, or after an agent's tmux session has crashed or
/// been killed. Requires a worktree to already exist for the agent -- creates
/// a fresh tmux session running Claude with empty context (no `--resume`).
///
/// Errors cleanly if the agent is already running (nothing to restart) or if
/// no worktree exists (nothing to restart *into* -- use `skulk new`).
pub(crate) fn cmd_restart(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;

    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let branch = agent.branch_name();
    let worktree = agent.worktree_path(cfg);

    let inv = fetch_inventory(ssh, cfg)?;

    if inv.sessions.contains(&session_name) {
        return Err(SkulkError::Validation(format!(
            "Agent '{name}' is already running. Use `skulk connect {name}` to attach."
        )));
    }
    if !inv.worktrees.contains_key(&session_name) {
        return Err(SkulkError::NotFound(format!(
            "No worktree for agent '{name}'. Use `skulk new {name}` to create a new agent."
        )));
    }

    ssh.run(&agent_create_tmux_command(name, cfg, false, None, None))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;

    println!(
        "Agent '{name}' restarted.\n\
         \x20 Branch: {branch}\n\
         \x20 Worktree: {worktree}\n\
         \x20 Mode: skip-permissions\n\
         \x20 Context: fresh (empty)\n\
         \n\
         Next steps:\n\
         \x20 skulk connect {name}    # attach to session\n\
         \x20 skulk send {name} \"...\" # send a prompt\n\
         \x20 skulk archive {name}    # stop without losing work"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, mock_empty_inventory, mock_inventory, mock_inventory_single_agent,
        ssh_ok, test_config,
    };

    #[test]
    fn cmd_restart_succeeds_for_archived_agent() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", &cfg).is_ok());
    }

    #[test]
    fn cmd_restart_succeeds_for_stopped_agent_without_branch() {
        // An orphaned worktree (no branch listed, but worktree exists) should
        // still be restartable -- the worktree is the source of truth for
        // "there's work here".
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &[],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", &cfg).is_ok());
    }

    #[test]
    fn cmd_restart_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_restart(&ssh, "../bad", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn cmd_restart_errors_when_session_already_running() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory_single_agent("skulk-task"))]);
        let result = cmd_restart(&ssh, "task", &cfg);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("already running"));
            assert!(msg.contains("skulk connect"));
        });
    }

    #[test]
    fn cmd_restart_errors_when_no_worktree_exists() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let result = cmd_restart(&ssh, "ghost", &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"));
            assert!(msg.contains("skulk new"));
        });
    }

    #[test]
    fn cmd_restart_errors_when_only_branch_exists_without_worktree() {
        // Dangling branch without its worktree -- restart has nothing to land in.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(&[], &[], &["skulk-dangling"]))]);
        let result = cmd_restart(&ssh, "dangling", &cfg);
        assert!(matches!(result, Err(SkulkError::NotFound(_))));
    }

    #[test]
    fn cmd_restart_launches_claude_via_tmux_in_worktree() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", &cfg).is_ok());
        let tmux_call = &ssh.calls()[1];
        assert!(tmux_call.contains("tmux new-session -d -s skulk-task"));
        assert!(tmux_call.contains("-c ~/test-project-worktrees/skulk-task"));
        assert!(tmux_call.contains("claude --dangerously-skip-permissions"));
    }

    #[test]
    fn cmd_restart_does_not_pass_remote_control_flag() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", &cfg).is_ok());
        let tmux_call = &ssh.calls()[1];
        assert!(
            !tmux_call.contains("--remote-control"),
            "restart should not enable remote-control, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_restart_surfaces_tmux_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            Err(SkulkError::SshFailed("tmux: server not responding".into())),
        ]);
        let result = cmd_restart(&ssh, "task", &cfg);
        assert!(matches!(result, Err(SkulkError::SshFailed(_))));
    }

    #[test]
    fn cmd_restart_surfaces_inventory_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection timed out.".into(),
            suggestion: "Check network.".into(),
        })]);
        let result = cmd_restart(&ssh, "task", &cfg);
        assert!(matches!(result, Err(SkulkError::Diagnostic { .. })));
    }

    #[test]
    fn cmd_restart_classifies_tmux_connection_error() {
        // A transient SSH error during tmux-create must be upgraded to a
        // Diagnostic (friendly message) via `classify_agent_error`, not leak
        // as raw SshFailed. Mirrors the classification applied elsewhere in
        // the command surface.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            Err(SkulkError::SshFailed("Connection timed out".into())),
        ]);
        let result = cmd_restart(&ssh, "task", &cfg);
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.to_lowercase().contains("timed out"));
        });
    }
}
