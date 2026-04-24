use crate::agent_ref::AgentRef;
use crate::commands::destroy::agent_destroy_session_command;
use crate::commands::new::agent_create_tmux_command;
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::{validate_model, validate_name};

/// Restart an agent in its existing worktree with a fresh Claude session.
///
/// Use after `skulk archive`, or after an agent's tmux session has crashed or
/// been killed. Requires a worktree to already exist for the agent -- creates
/// a fresh tmux session running Claude with empty context (no `--resume`).
///
/// Errors cleanly if the agent is already running (nothing to restart) or if
/// no worktree exists (nothing to restart *into* -- use `skulk new`).
///
/// `remote_control`, `model`, and `claude_args` mirror the corresponding flags
/// on `skulk new` — see [`agent_create_tmux_command`] for the caller's quoting
/// responsibilities on `claude_args`.
pub(crate) fn cmd_restart(
    ssh: &impl Ssh,
    name: &str,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    if let Some(m) = model {
        validate_model(m)?;
    }

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

    // Tmux creation is a two-step shell pipeline (`new-session && send-keys`).
    // If `new-session` succeeds but `send-keys` fails, we leave an empty
    // session that would block a subsequent restart with "already running".
    // Best-effort kill on failure so retry stays clean; swallow the cleanup
    // error (the original failure is the one the user needs to see).
    if let Err(e) = ssh.run(&agent_create_tmux_command(
        name,
        cfg,
        remote_control,
        model,
        claude_args,
    )) {
        if ssh.run(&agent_destroy_session_command(name, cfg)).is_err() {
            eprintln!(
                "Warning: failed to clean up partial tmux session for agent '{name}'. \
                 If `skulk restart {name}` reports 'already running', run `skulk destroy {name}` first."
            );
        }
        return Err(classify_agent_error(name, e, &cfg.host));
    }

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
        assert!(cmd_restart(&ssh, "task", false, None, None, &cfg).is_ok());
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
        assert!(cmd_restart(&ssh, "task", false, None, None, &cfg).is_ok());
    }

    #[test]
    fn cmd_restart_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_restart(&ssh, "../bad", false, None, None, &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn cmd_restart_errors_when_session_already_running() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory_single_agent("skulk-task"))]);
        let result = cmd_restart(&ssh, "task", false, None, None, &cfg);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("already running"));
            assert!(msg.contains("skulk connect"));
        });
    }

    #[test]
    fn cmd_restart_errors_when_no_worktree_exists() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let result = cmd_restart(&ssh, "ghost", false, None, None, &cfg);
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
        let result = cmd_restart(&ssh, "dangling", false, None, None, &cfg);
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
        assert!(cmd_restart(&ssh, "task", false, None, None, &cfg).is_ok());
        let tmux_call = &ssh.calls()[1];
        assert!(tmux_call.contains("tmux new-session -d -s skulk-task"));
        assert!(tmux_call.contains("-c ~/test-project-worktrees/skulk-task"));
        assert!(tmux_call.contains("claude --dangerously-skip-permissions"));
    }

    #[test]
    fn cmd_restart_without_remote_control_flag_omits_flag() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", false, None, None, &cfg).is_ok());
        let tmux_call = &ssh.calls()[1];
        assert!(
            !tmux_call.contains("--remote-control"),
            "restart should not enable remote-control by default, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_restart_with_remote_control_flag_passes_flag_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", true, None, None, &cfg).is_ok());
        let tmux_call = &ssh.calls()[1];
        assert!(
            tmux_call.contains("--remote-control skulk-task"),
            "tmux launch command should include --remote-control when flag is true, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_restart_with_model_flag_passes_flag_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", false, Some("opus"), None, &cfg).is_ok());
        let tmux_call = &ssh.calls()[1];
        assert!(
            tmux_call.contains("--model opus"),
            "tmux launch command should include --model opus, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_restart_rejects_invalid_model_before_ssh() {
        let cfg = test_config();
        // No SSH responses queued -- validation must fail before any SSH call.
        let ssh = MockSsh::new(vec![]);
        let result = cmd_restart(&ssh, "task", false, Some("opus; rm -rf /"), None, &cfg);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("Invalid character"));
        });
        assert!(
            ssh.calls().is_empty(),
            "validation must short-circuit before any SSH call, got: {:?}",
            ssh.calls()
        );
    }

    #[test]
    fn cmd_restart_with_claude_args_passes_args_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(
            cmd_restart(
                &ssh,
                "task",
                false,
                None,
                Some("--allowed-tools Bash"),
                &cfg
            )
            .is_ok()
        );
        let tmux_call = &ssh.calls()[1];
        assert!(
            tmux_call.contains("--allowed-tools Bash"),
            "tmux launch command should include extra claude args, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_restart_threads_all_flags_into_single_tmux_call() {
        // Regression guard against a positional swap of `model` and
        // `claude_args` at the cmd_restart -> agent_create_tmux_command
        // boundary: single-flag tests would still pass under a swap, so
        // assert all three wires land in the same tmux invocation.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            ssh_ok(),
        ]);
        assert!(cmd_restart(&ssh, "task", true, Some("opus"), Some("--verbose"), &cfg).is_ok());
        let tmux_call = &ssh.calls()[1];
        assert!(
            tmux_call.contains("--remote-control skulk-task"),
            "combined launch should include --remote-control, got: {tmux_call}"
        );
        assert!(
            tmux_call.contains("--model opus"),
            "combined launch should include --model opus, got: {tmux_call}"
        );
        assert!(
            tmux_call.contains("--verbose"),
            "combined launch should include extra claude args, got: {tmux_call}"
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
            // Cleanup kill-session attempt after tmux creation failed.
            ssh_ok(),
        ]);
        let result = cmd_restart(&ssh, "task", false, None, None, &cfg);
        assert!(matches!(result, Err(SkulkError::SshFailed(_))));
    }

    #[test]
    fn cmd_restart_tmux_failure_kills_partial_session() {
        // If tmux new-session succeeded but send-keys failed, we would leave an
        // empty session that blocks a retry with "already running". Cleanup
        // must attempt to kill the partial session so retry stays clean.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            Err(SkulkError::SshFailed("send-keys failed".into())),
            ssh_ok(), // cleanup kill-session succeeds
        ]);
        let result = cmd_restart(&ssh, "task", false, None, None, &cfg);
        assert!(result.is_err());
        let calls = ssh.calls();
        assert_eq!(
            calls.len(),
            3,
            "expected inventory + tmux + cleanup: {calls:?}"
        );
        assert!(
            calls[2].contains("tmux kill-session -t skulk-task"),
            "cleanup must kill the partial session: {}",
            calls[2]
        );
    }

    #[test]
    fn cmd_restart_tmux_failure_cleanup_failure_still_surfaces_original_error() {
        // If the cleanup kill-session also fails, the ORIGINAL tmux error is
        // what the user needs to see (the cleanup error is just noise).
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-task", "/path/skulk-task")],
                &["skulk-task"],
            )),
            Err(SkulkError::SshFailed("original send-keys failure".into())),
            Err(SkulkError::SshFailed("cleanup also failed".into())),
        ]);
        let result = cmd_restart(&ssh, "task", false, None, None, &cfg);
        assert_err!(result, SkulkError::SshFailed(msg) => {
            assert!(
                msg.contains("original send-keys failure"),
                "original error must surface, not cleanup error: {msg}"
            );
        });
    }

    #[test]
    fn cmd_restart_surfaces_inventory_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection timed out.".into(),
            suggestion: "Check network.".into(),
        })]);
        let result = cmd_restart(&ssh, "task", false, None, None, &cfg);
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
            // Cleanup attempt after tmux failure -- likely also times out in
            // reality but we just need it to be consumed by the mock.
            ssh_ok(),
        ]);
        let result = cmd_restart(&ssh, "task", false, None, None, &cfg);
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.to_lowercase().contains("timed out"));
        });
    }
}
