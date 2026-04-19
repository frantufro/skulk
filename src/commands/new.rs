use crate::config::Config;
use crate::error::SkulkError;
use crate::inventory::{inventory_command, parse_inventory};
use crate::ssh::Ssh;
use crate::util::{PromptStatus, STARTUP_DELAY, shell_escape, validate_model, validate_name};

/// Build the SSH command to create a git worktree for an agent.
pub(crate) fn agent_create_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;
    let default_branch = &cfg.default_branch;
    format!(
        "mkdir -p {worktree_base} && cd {base_path} && git worktree add -b {session_prefix}{name} {worktree_base}/{session_prefix}{name} {default_branch}"
    )
}

/// Build the SSH command to create a tmux session and launch Claude Code for an agent.
///
/// Creates the session with a login shell (not a direct command) so the session
/// survives if Claude exits, then sends the claude command via send-keys.
/// Using a login shell also ensures ~/.local/bin is in PATH.
///
/// When `remote_control` is true, Claude is launched with `--remote-control`
/// so the agent is reachable from the Claude Code mobile/web app. Off by default
/// because it triggers an upstream idle-death bug
/// (<https://github.com/anthropics/claude-code/issues/32982>); Skulk's own
/// commands work through tmux directly and do not need it.
///
/// When `model` is provided, Claude is launched with `--model <name>` so the
/// agent runs on a specific model (e.g. `opus`, `sonnet`, `claude-opus-4-7`).
/// The caller must pre-validate the value with `validate_model`; we only
/// escape for the outer single-quoted `send-keys` argument here.
///
/// When `claude_args` is provided, the raw string is appended to the Claude
/// command line. IMPORTANT: `tmux send-keys` *types* this string into the
/// remote shell, which then re-parses it — shell metacharacters (`$`,
/// `` ` ``, `;`, `(`, `)`, globs, unquoted whitespace) are re-evaluated by
/// that shell. Callers must pre-quote values that must reach Claude
/// literally (e.g. `--allowed-tools 'Bash(gh pr:*)'` — note the inner
/// single quotes). `shell_escape` here only protects the outer `send-keys`
/// quoting, not the inner shell.
pub(crate) fn agent_create_tmux_command(
    name: &str,
    cfg: &Config,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
) -> String {
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;
    let remote_control_flag = if remote_control {
        format!(" --remote-control {session_prefix}{name}")
    } else {
        String::new()
    };
    let model_flag = match model {
        Some(m) => format!(" --model {}", shell_escape(m)),
        None => String::new(),
    };
    let extra_args = match claude_args {
        Some(args) if !args.is_empty() => format!(" {}", shell_escape(args)),
        _ => String::new(),
    };
    format!(
        "tmux new-session -d -s {session_prefix}{name} -c {worktree_base}/{session_prefix}{name} && \
         tmux send-keys -t {session_prefix}{name} \
         'claude --dangerously-skip-permissions{remote_control_flag}{model_flag}{extra_args}' C-m"
    )
}

/// Build the SSH command to send an initial prompt to an agent after a startup delay.
/// The sleep runs on the remote so it does not block the laptop CLI.
/// Checks that the session is still alive after sleeping before attempting delivery,
/// so it fails cleanly if Claude Code exited during startup.
///
/// Uses a tmux named buffer + `paste-buffer -p` (bracketed paste) so multi-line
/// prompts arrive as a single pasted block rather than line-by-line — each newline
/// in a `send-keys` call would otherwise submit a partial message to Claude Code.
/// After the paste we sleep briefly then send Enter separately, which defeats
/// Claude Code's paste-detection swallowing the trailing Enter as a newline.
///
/// `paste-buffer -d` deletes the buffer atomically with a successful paste, so a
/// prompt containing sensitive context never lingers in tmux's server-wide buffer
/// list. The buffer name is deterministic per agent (`skulk-prompt-<session>`),
/// so even in the rare race where the session dies between `has-session` and
/// `paste-buffer` and the buffer leaks, the next attempt for the same agent
/// overwrites it via `set-buffer`.
pub(crate) fn agent_send_prompt_command(name: &str, prompt: &str, cfg: &Config) -> String {
    let escaped = shell_escape(prompt);
    let session_prefix = &cfg.session_prefix;
    let buffer = format!("skulk-prompt-{session_prefix}{name}");
    format!(
        "sleep {STARTUP_DELAY} && tmux has-session -t {session_prefix}{name} && \
         tmux set-buffer -b {buffer} -- '{escaped}' && \
         tmux paste-buffer -p -d -t {session_prefix}{name} -b {buffer} && \
         sleep 0.1 && \
         tmux send-keys -t {session_prefix}{name} Enter"
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
/// 5. Create tmux session with Claude Code (with `--remote-control` if requested)
///    - On failure: rollback worktree
/// 6. Send prompt if provided
///    - On failure: warn user, keep agent alive
/// 7. Print success output
pub(crate) fn cmd_new(
    ssh: &impl Ssh,
    name: &str,
    prompt: Option<&str>,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let base_path = &cfg.base_path;
    let host = &cfg.host;
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;

    // Step 0: Validate name and (if provided) model
    validate_name(name)?;
    if let Some(m) = model {
        validate_model(m)?;
    }

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
    let has_session = inv.sessions.contains(&session_name);
    let has_worktree = inv.worktrees.contains_key(&session_name);
    if has_session {
        return Err(SkulkError::Validation(format!(
            "Agent '{name}' already exists."
        )));
    }
    if has_worktree {
        return Err(SkulkError::Validation(format!(
            "Agent '{name}' already has a worktree (archived or crashed).\n  \
             Resume it: `skulk restart {name}`\n  \
             Or wipe it: `skulk destroy {name}`"
        )));
    }

    // Step 3: Create worktree
    ssh.run(&agent_create_worktree_command(name, cfg))?;

    // Step 4: Create tmux session with Claude Code
    if let Err(e) = ssh.run(&agent_create_tmux_command(
        name,
        cfg,
        remote_control,
        model,
        claude_args,
    )) {
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

    let permissions_line = if remote_control {
        "  Mode: remote-control (skip-permissions)"
    } else {
        "  Mode: skip-permissions"
    };

    let model_line = model.map(|m| format!("\n  Model: {m}")).unwrap_or_default();
    let extra_args_line = claude_args
        .filter(|s| !s.is_empty())
        .map(|a| format!("\n  Extra Claude args: {a}"))
        .unwrap_or_default();

    println!(
        "Agent '{name}' created.\n\
         \x20 Branch: {session_prefix}{name}\n\
         \x20 Worktree: {worktree_base}/{session_prefix}{name}\n\
         {permissions_line}{model_line}{extra_args_line}\n\
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
    fn agent_create_tmux_command_with_remote_control_includes_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, true, None, None);
        assert!(cmd.contains("tmux new-session -d -s skulk-my-task"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--remote-control skulk-my-task"));
    }

    #[test]
    fn agent_create_tmux_command_without_remote_control_omits_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, None, None);
        assert!(cmd.contains("tmux new-session -d -s skulk-my-task"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(
            !cmd.contains("--remote-control"),
            "flag should be absent when remote_control=false, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_with_model_includes_model_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, Some("opus"), None);
        assert!(
            cmd.contains("--model opus"),
            "should include --model flag, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_without_model_omits_model_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, None, None);
        assert!(
            !cmd.contains("--model"),
            "should omit --model flag when model is None, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_with_claude_args_appends_verbatim() {
        let cfg = test_config();
        let cmd =
            agent_create_tmux_command("my-task", &cfg, false, None, Some("--allowed-tools Bash"));
        assert!(
            cmd.contains("--allowed-tools Bash"),
            "should append claude_args, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_with_empty_claude_args_appends_nothing() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, None, Some(""));
        assert!(
            cmd.contains("'claude --dangerously-skip-permissions'"),
            "empty claude_args should not introduce trailing space, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_claude_args_escapes_single_quotes() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command(
            "my-task",
            &cfg,
            false,
            None,
            Some("--system-prompt 'be nice'"),
        );
        // Single quotes in claude_args must be escaped for the surrounding single-quoted send-keys string.
        assert!(
            cmd.contains("--system-prompt '\\''be nice'\\''"),
            "single quotes in claude_args should be POSIX-escaped, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_claude_args_shell_metacharacters_pass_through_literally() {
        // Pins intentional design: the outer single-quote wrap keeps metacharacters
        // literal for the SSH + send-keys layer. The string is then typed into the
        // inner tmux shell which DOES re-evaluate them — callers pre-quote for that
        // shell themselves (documented on the flag and in README).
        let cfg = test_config();
        let cmd = agent_create_tmux_command(
            "my-task",
            &cfg,
            false,
            None,
            Some("--flag $(whoami) `id` ; true"),
        );
        assert!(
            cmd.contains("$(whoami)") && cmd.contains("`id`") && cmd.contains("; true"),
            "metacharacters must survive the outer layer verbatim; got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_model_and_claude_args_combine() {
        let cfg = test_config();
        let cmd =
            agent_create_tmux_command("my-task", &cfg, true, Some("sonnet"), Some("--verbose"));
        assert!(cmd.contains("--remote-control skulk-my-task"));
        assert!(cmd.contains("--model sonnet"));
        assert!(cmd.contains("--verbose"));
    }

    #[test]
    fn agent_send_prompt_command_simple() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "fix the bug", &cfg);
        assert!(cmd.contains("sleep 5"));
        assert!(cmd.contains("tmux set-buffer"));
        assert!(cmd.contains("tmux paste-buffer -p"));
        assert!(cmd.contains("'fix the bug'"));
        assert!(cmd.contains("Enter"));
    }

    #[test]
    fn agent_send_prompt_command_splits_paste_and_submit() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "fix the bug", &cfg);
        let paste_idx = cmd.find("tmux paste-buffer").expect("paste step missing");
        let submit_sleep_idx = cmd[paste_idx..]
            .find("sleep 0.1")
            .map(|i| i + paste_idx)
            .expect("submit delay missing");
        let enter_idx = cmd[submit_sleep_idx..]
            .find("send-keys")
            .map(|i| i + submit_sleep_idx)
            .expect("submit send-keys missing");
        assert!(paste_idx < submit_sleep_idx);
        assert!(submit_sleep_idx < enter_idx);
        assert!(cmd[enter_idx..].contains("Enter"));
    }

    #[test]
    fn agent_send_prompt_command_with_quotes() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "it's broken", &cfg);
        assert!(cmd.contains("it'\\''s broken"));
    }

    #[test]
    fn agent_send_prompt_command_handles_multiline() {
        let cfg = test_config();
        let prompt = "Line 1\nLine 2\nLine 3";
        let cmd = agent_send_prompt_command("my-task", prompt, &cfg);
        // The full prompt lives inside a single tmux set-buffer argument, so
        // delivery is one bracketed-paste block — newlines do not submit.
        assert!(cmd.contains("'Line 1\nLine 2\nLine 3'"));
        assert!(cmd.contains("paste-buffer -p"));
    }

    #[test]
    fn agent_send_prompt_command_deletes_buffer_atomically_with_paste() {
        // `paste-buffer -d` deletes the buffer as part of a successful paste so the
        // prompt content does not linger in tmux's server-wide buffer list. We assert
        // the `-d` flag is on the same `paste-buffer` invocation, not a separate
        // `delete-buffer` call (which could leak the buffer if paste failed first).
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "hi", &cfg);
        assert!(
            cmd.contains("paste-buffer -p -d"),
            "expected paste-buffer to use -d for atomic delete-on-paste, got: {cmd}"
        );
        assert!(
            !cmd.contains("delete-buffer"),
            "separate delete-buffer call should be gone in favor of paste-buffer -d"
        );
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
        assert!(cmd_new(&ssh, "test", None, false, None, None, &cfg).is_ok());
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
        assert!(cmd_new(&ssh, "test", Some("fix the bug"), false, None, None, &cfg).is_ok());
    }

    #[test]
    fn cmd_new_with_remote_control_flag_passes_flag_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_new(&ssh, "test", None, true, None, None, &cfg).is_ok());
        // Fourth SSH call is the tmux-create command; verify the flag landed there.
        let tmux_call = &ssh.calls()[3];
        assert!(
            tmux_call.contains("--remote-control skulk-test"),
            "tmux launch command should include --remote-control when flag is true, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_new_without_remote_control_flag_omits_flag() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_new(&ssh, "test", None, false, None, None, &cfg).is_ok());
        let tmux_call = &ssh.calls()[3];
        assert!(
            !tmux_call.contains("--remote-control"),
            "tmux launch command should omit --remote-control when flag is false, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_new_with_model_flag_passes_flag_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_new(&ssh, "test", None, false, Some("opus"), None, &cfg).is_ok());
        let tmux_call = &ssh.calls()[3];
        assert!(
            tmux_call.contains("--model opus"),
            "tmux launch command should include --model opus, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_new_rejects_invalid_model_before_ssh() {
        let cfg = test_config();
        // No SSH responses queued — validation must fail before any SSH call.
        let ssh = MockSsh::new(vec![]);
        let result = cmd_new(
            &ssh,
            "test",
            None,
            false,
            Some("opus; rm -rf /"),
            None,
            &cfg,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => assert!(msg.contains("Invalid character")),
            other => panic!("expected Validation, got: {other}"),
        }
        assert!(
            ssh.calls().is_empty(),
            "validation must short-circuit before any SSH call, got: {:?}",
            ssh.calls()
        );
    }

    #[test]
    fn cmd_new_with_claude_args_passes_args_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(
            cmd_new(
                &ssh,
                "test",
                None,
                false,
                None,
                Some("--allowed-tools Bash"),
                &cfg
            )
            .is_ok()
        );
        let tmux_call = &ssh.calls()[3];
        assert!(
            tmux_call.contains("--allowed-tools Bash"),
            "tmux launch command should include extra claude_args, got: {tmux_call}"
        );
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
        let result = cmd_new(&ssh, "dupe", None, false, None, None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => assert!(msg.contains("already exists")),
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_new_existing_worktree_suggests_restart_or_destroy() {
        let cfg = test_config();
        // No session, but worktree exists -- archived agent or crashed `new`.
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(
                &[],
                &[("skulk-zombie", "/path/skulk-zombie")],
                &["skulk-zombie"],
            )),
        ]);
        let result = cmd_new(&ssh, "zombie", None, false, None, None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => {
                assert!(
                    msg.contains("skulk restart zombie"),
                    "should suggest restart: {msg}"
                );
                assert!(
                    msg.contains("skulk destroy zombie"),
                    "should suggest destroy: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_new_missing_base_clone_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("test failed".into()))]);
        let result = cmd_new(&ssh, "test", None, false, None, None, &cfg);
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
        let result = cmd_new(&ssh, "test", None, false, None, None, &cfg);
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
        let result = cmd_new(&ssh, "test", None, false, None, None, &cfg);
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
        let result = cmd_new(&ssh, "test", Some("fix the bug"), false, None, None, &cfg);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_new_base_clone_check_connectivity_error_propagated() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection refused.".into(),
            suggestion: "SSH not running.".into(),
        })]);
        let result = cmd_new(&ssh, "test", None, false, None, None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Diagnostic { message, .. } => assert!(message.contains("refused")),
            other => panic!("expected Diagnostic, got: {other}"),
        }
    }
}
