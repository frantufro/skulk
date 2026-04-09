use crate::config::Config;
use crate::error::SkulkError;
use crate::inventory::{inventory_command, parse_inventory};
use crate::ssh::Ssh;
use crate::util::{PromptStatus, STARTUP_DELAY, shell_escape, validate_name};

/// Build the SSH command to create a git worktree for an agent.
pub(crate) fn agent_create_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;
    format!(
        "mkdir -p {worktree_base} && cd {base_path} && git worktree add -b {session_prefix}{name} {worktree_base}/{session_prefix}{name} main"
    )
}

/// Build the SSH command to create a tmux session and launch Claude Code for an agent.
///
/// Creates the session with a login shell (not a direct command) so the session
/// survives if Claude exits, then sends the claude command via send-keys.
/// Using a login shell also ensures ~/.local/bin is in PATH.
pub(crate) fn agent_create_tmux_command(name: &str, cfg: &Config) -> String {
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;
    format!(
        "tmux new-session -d -s {session_prefix}{name} -c {worktree_base}/{session_prefix}{name} && \
         tmux send-keys -t {session_prefix}{name} \
         'claude --dangerously-skip-permissions --remote-control {session_prefix}{name}' C-m"
    )
}

/// Build the SSH command to send an initial prompt to an agent after a startup delay.
/// The sleep runs on the remote so it does not block the laptop CLI.
/// Checks that the session is still alive after sleeping before attempting send-keys,
/// so it fails cleanly if Claude Code exited during startup.
pub(crate) fn agent_send_prompt_command(name: &str, prompt: &str, cfg: &Config) -> String {
    let escaped = shell_escape(prompt);
    let session_prefix = &cfg.session_prefix;
    format!(
        "sleep {STARTUP_DELAY} && tmux has-session -t {session_prefix}{name} && \
         tmux send-keys -t {session_prefix}{name} '{escaped}' C-m"
    )
}

/// Build the SSH command to roll back a worktree creation (remove worktree + delete branch).
pub(crate) fn agent_rollback_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;
    format!(
        "cd {base_path} && git worktree remove --force {worktree_base}/{session_prefix}{name} && git branch -D {session_prefix}{name}"
    )
}

/// Create a new agent with worktree isolation and optional initial prompt.
///
/// Orchestration sequence:
/// 1. Validate name
/// 2. Check base clone exists
/// 3. Fetch inventory and check uniqueness
/// 4. Create worktree
/// 5. Create tmux session with Claude Code
///    - On failure: rollback worktree
/// 6. Send prompt if provided
///    - On failure: warn user, keep agent alive
/// 7. Print success output
pub(crate) fn cmd_new(
    ssh: &impl Ssh,
    name: &str,
    prompt: Option<&str>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let base_path = &cfg.base_path;
    let host = &cfg.host;
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;

    // Step 0: Validate name
    validate_name(name)?;

    // Step 1: Check base clone exists
    match ssh.run(&format!("test -d {base_path}/.git && echo exists")) {
        Ok(_) => {}
        Err(SkulkError::SshFailed(_)) => {
            return Err(SkulkError::Validation(format!(
                "Base clone not found at {base_path} on {host}. Run `skulk pull` or clone manually."
            )));
        }
        Err(e) => return Err(e),
    }

    // Step 2: Fetch inventory and check uniqueness
    let inv = parse_inventory(&ssh.run(&inventory_command(cfg))?, cfg);
    let session_name = format!("{session_prefix}{name}");
    if inv.sessions.contains(&session_name) || inv.worktrees.contains_key(&session_name) {
        return Err(SkulkError::Validation(format!(
            "Agent '{name}' already exists."
        )));
    }

    // Step 3: Create worktree
    ssh.run(&agent_create_worktree_command(name, cfg))?;

    // Step 4: Create tmux session with Claude Code
    if let Err(e) = ssh.run(&agent_create_tmux_command(name, cfg)) {
        // Attempt rollback (best-effort)
        if ssh
            .run(&agent_rollback_worktree_command(name, cfg))
            .is_err()
        {
            eprintln!(
                "Warning: Failed to clean up worktree for agent '{name}'. Run `skulk gc` to clean up."
            );
        }
        return Err(e);
    }

    // Step 5: Send prompt if provided
    let prompt_status = if let Some(prompt_text) = prompt {
        if ssh
            .run(&agent_send_prompt_command(name, prompt_text, cfg))
            .is_ok()
        {
            PromptStatus::Delivered
        } else {
            eprintln!(
                "Warning: Agent '{name}' created but prompt delivery failed. Send manually:\n  skulk send {name} \"...\""
            );
            PromptStatus::Failed
        }
    } else {
        PromptStatus::NotSent
    };

    // Step 6: Print success output
    let prompt_line = match prompt_status {
        PromptStatus::Delivered => format!("  Prompt: delivered (after {STARTUP_DELAY}s delay)"),
        PromptStatus::Failed => {
            "  Prompt: delivery failed (agent is running, send manually)".to_string()
        }
        PromptStatus::NotSent => "  Prompt: none (idle, waiting for input)".to_string(),
    };

    println!(
        "Agent '{name}' created.\n\
         \x20 Branch: {session_prefix}{name}\n\
         \x20 Worktree: {worktree_base}/{session_prefix}{name}\n\
         \x20 Mode: remote-control (skip-permissions)\n\
         {prompt_line}\n\
         \n\
         Next steps:\n\
         \x20 skulk connect {name}    # attach to session\n\
         \x20 skulk send {name} \"...\" # send a prompt\n\
         \x20 skulk destroy {name}    # tear down"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, mock_inventory, test_config};

    #[test]
    fn agent_create_worktree_command_generates_correct_shell() {
        let cfg = test_config();
        let cmd = agent_create_worktree_command("my-task", &cfg);
        assert!(cmd.contains("mkdir -p ~/test-project-worktrees"));
        assert!(cmd.contains("git worktree add -b skulk-my-task"));
        assert!(cmd.contains("~/test-project-worktrees/skulk-my-task"));
        assert!(cmd.contains("main"));
    }

    #[test]
    fn agent_create_tmux_command_generates_correct_shell() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg);
        assert!(cmd.contains("tmux new-session -d -s skulk-my-task"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--remote-control skulk-my-task"));
    }

    #[test]
    fn agent_send_prompt_command_simple() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "fix the bug", &cfg);
        assert!(cmd.contains("sleep 5"));
        assert!(cmd.contains("tmux send-keys -t skulk-my-task"));
        assert!(cmd.contains("'fix the bug'"));
        assert!(cmd.contains("C-m"));
    }

    #[test]
    fn agent_send_prompt_command_with_quotes() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "it's broken", &cfg);
        assert!(cmd.contains("it'\\''s broken"));
    }

    #[test]
    fn agent_rollback_worktree_command_generates() {
        let cfg = test_config();
        let cmd = agent_rollback_worktree_command("my-task", &cfg);
        assert!(cmd.contains("git worktree remove --force"));
        assert!(cmd.contains("~/test-project-worktrees/skulk-my-task"));
        assert!(cmd.contains("git branch -D skulk-my-task"));
    }

    #[test]
    fn cmd_new_succeeds_without_prompt() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_new(&ssh, "test", None, &cfg).is_ok());
    }

    #[test]
    fn cmd_new_succeeds_with_prompt() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_new(&ssh, "test", Some("fix the bug"), &cfg).is_ok());
    }

    #[test]
    fn cmd_new_duplicate_agent_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(
                &["skulk-dupe"],
                &[("skulk-dupe", "/path/skulk-dupe")],
                &["skulk-dupe"],
            )),
        ]);
        let result = cmd_new(&ssh, "dupe", None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => assert!(msg.contains("already exists")),
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_new_missing_base_clone_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("test failed".into()))]);
        let result = cmd_new(&ssh, "test", None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => assert!(msg.contains("Base clone not found")),
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_new_tmux_failure_rolls_back() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Err(SkulkError::SshFailed("tmux failed".into())),
            Ok(String::new()),
        ]);
        let result = cmd_new(&ssh, "test", None, &cfg);
        assert!(result.is_err());
    }

    #[test]
    fn cmd_new_tmux_fails_rollback_also_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Err(SkulkError::SshFailed("tmux creation failed".into())),
            Err(SkulkError::SshFailed("rollback also failed".into())),
        ]);
        let result = cmd_new(&ssh, "test", None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::SshFailed(msg) => assert!(msg.contains("tmux creation failed")),
            other => panic!("expected SshFailed, got: {other}"),
        }
    }

    #[test]
    fn cmd_new_prompt_delivery_fails_still_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
            Err(SkulkError::SshFailed("send-keys failed".into())),
        ]);
        let result = cmd_new(&ssh, "test", Some("fix the bug"), &cfg);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_new_base_clone_check_connectivity_error_propagated() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection refused.".into(),
            suggestion: "SSH not running.".into(),
        })]);
        let result = cmd_new(&ssh, "test", None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Diagnostic { message, .. } => assert!(message.contains("refused")),
            other => panic!("expected Diagnostic, got: {other}"),
        }
    }
}
