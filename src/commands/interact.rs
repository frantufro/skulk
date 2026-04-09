use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::ssh::Ssh;
use crate::util::{shell_escape, validate_name};

/// Build the SSH command to attach to an agent's live tmux session.
pub(crate) fn connect_command(name: &str, cfg: &Config) -> String {
    let session_prefix = &cfg.session_prefix;
    format!("tmux attach-session -t {session_prefix}{name}")
}

/// Build the SSH command to capture the visible pane content of an agent's tmux session.
///
/// Used by both `logs` (snapshot mode) and `send` (delivery verification).
pub(crate) fn capture_pane_command(name: &str, cfg: &Config) -> String {
    let session_prefix = &cfg.session_prefix;
    format!("tmux capture-pane -p -t {session_prefix}{name}")
}

/// Build the SSH command to capture N lines of scrollback from an agent's tmux session.
pub(crate) fn logs_snapshot_deep_command(name: &str, lines: u32, cfg: &Config) -> String {
    let session_prefix = &cfg.session_prefix;
    format!("tmux capture-pane -p -t {session_prefix}{name} -S -{lines}")
}

/// Build the SSH command to send a prompt to a running agent (no startup delay).
///
/// Unlike `agent_send_prompt_command()` (in new.rs) which includes a startup delay,
/// this targets an already-running agent -- no delay needed.
pub(crate) fn send_command(name: &str, prompt: &str, cfg: &Config) -> String {
    let escaped = shell_escape(prompt);
    let session_prefix = &cfg.session_prefix;
    format!("tmux send-keys -t {session_prefix}{name} '{escaped}' C-m")
}

/// Attach to an agent's live tmux session via interactive SSH.
pub(crate) fn cmd_connect(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    let session_prefix = &cfg.session_prefix;
    // Pre-check that the session exists before launching interactive SSH,
    // because ssh_interactive replaces the process and tmux errors don't
    // propagate as non-zero exit codes in non-interactive contexts.
    let check = format!("tmux has-session -t {session_prefix}{name}");
    ssh.run(&check)
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    let cmd = connect_command(name, cfg);
    let status = ssh.interactive(&cmd)?;
    if status.success() {
        eprintln!("Detached from {session_prefix}{name}.");
    } else {
        eprintln!("Connection to {session_prefix}{name} ended (non-zero exit).");
    }
    Ok(())
}

/// View agent output: snapshot, deep snapshot (--lines), or follow mode (--follow).
pub(crate) fn cmd_logs(
    ssh: &impl Ssh,
    name: &str,
    follow: bool,
    lines: Option<u32>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    if follow {
        return crate::io::cmd_logs_follow(ssh, name, cfg);
    }
    let cmd = match lines {
        Some(n) => logs_snapshot_deep_command(name, n, cfg),
        None => capture_pane_command(name, cfg),
    };
    let output = ssh
        .run(&cmd)
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    print!("{output}");
    Ok(())
}

/// Send a prompt to a running agent with delivery verification.
pub(crate) fn cmd_send(
    ssh: &impl Ssh,
    name: &str,
    prompt: &str,
    cfg: &Config,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    let session_prefix = &cfg.session_prefix;
    let host = &cfg.host;
    let before = ssh
        .run(&capture_pane_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;
    ssh.run(&send_command(name, prompt, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    match ssh.run(&capture_pane_command(name, cfg)) {
        Ok(after) if after == before => {
            eprintln!(
                "Warning: Prompt sent to {session_prefix}{name} but delivery could not be confirmed."
            );
        }
        Ok(_) => {
            eprintln!("Prompt delivered to {session_prefix}{name}.");
        }
        Err(_) => {
            eprintln!("Warning: Prompt sent, but post-send verification failed.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, test_config};

    #[test]
    fn connect_command_generates_tmux_attach() {
        let cfg = test_config();
        let cmd = connect_command("my-task", &cfg);
        assert_eq!(cmd, "tmux attach-session -t skulk-my-task");
    }

    #[test]
    fn connect_command_no_sleep() {
        let cfg = test_config();
        let cmd = connect_command("my-task", &cfg);
        assert!(!cmd.contains("sleep"));
    }

    #[test]
    fn connect_command_uses_session_prefix() {
        let cfg = test_config();
        let cmd = connect_command("test", &cfg);
        assert!(cmd.contains(&*cfg.session_prefix));
    }

    #[test]
    fn capture_pane_command_generates_capture() {
        let cfg = test_config();
        let cmd = capture_pane_command("my-task", &cfg);
        assert_eq!(cmd, "tmux capture-pane -p -t skulk-my-task");
    }

    #[test]
    fn capture_pane_command_includes_print_flag() {
        let cfg = test_config();
        let cmd = capture_pane_command("my-task", &cfg);
        assert!(cmd.contains("-p"));
    }

    #[test]
    fn logs_snapshot_deep_command_generates_capture_with_scrollback() {
        let cfg = test_config();
        let cmd = logs_snapshot_deep_command("my-task", 500, &cfg);
        assert_eq!(cmd, "tmux capture-pane -p -t skulk-my-task -S -500");
    }

    #[test]
    fn logs_snapshot_deep_command_includes_print_flag() {
        let cfg = test_config();
        let cmd = logs_snapshot_deep_command("my-task", 200, &cfg);
        assert!(cmd.contains("-p"));
    }

    #[test]
    fn logs_snapshot_deep_command_includes_scrollback_flag() {
        let cfg = test_config();
        let cmd = logs_snapshot_deep_command("my-task", 500, &cfg);
        assert!(cmd.contains("-S -500"));
    }

    #[test]
    fn send_command_generates_send_keys() {
        let cfg = test_config();
        let cmd = send_command("my-task", "fix the bug", &cfg);
        assert_eq!(cmd, "tmux send-keys -t skulk-my-task 'fix the bug' C-m");
    }

    #[test]
    fn send_command_excludes_sleep() {
        let cfg = test_config();
        let cmd = send_command("my-task", "fix the bug", &cfg);
        assert!(!cmd.contains("sleep"));
    }

    #[test]
    fn send_command_escapes_single_quotes() {
        let cfg = test_config();
        let cmd = send_command("my-task", "it's broken", &cfg);
        assert!(cmd.contains("it'\\''s broken"));
    }

    #[test]
    fn cmd_connect_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        assert!(cmd_connect(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_connect_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_connect(&ssh, "ghost", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_logs_snapshot_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("line 1\nline 2\nline 3".into())]);
        assert!(cmd_logs(&ssh, "test", false, None, &cfg).is_ok());
    }

    #[test]
    fn cmd_logs_deep_snapshot_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("scrollback line 1\nscrollback line 2".into())]);
        assert!(cmd_logs(&ssh, "test", false, Some(500), &cfg).is_ok());
    }

    #[test]
    fn cmd_logs_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-nope".into(),
        ))]);
        let result = cmd_logs(&ssh, "nope", false, None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("nope")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_logs_pane_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find pane: skulk-missing".into(),
        ))]);
        let result = cmd_logs(&ssh, "missing", false, None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("missing")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_send_succeeds_with_delivery_confirmed() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("old pane content".into()),
            Ok(String::new()),
            Ok("new pane content".into()),
        ]);
        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg).is_ok());
    }

    #[test]
    fn cmd_send_succeeds_with_unconfirmed_delivery() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("same pane content".into()),
            Ok(String::new()),
            Ok("same pane content".into()),
        ]);
        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg).is_ok());
    }

    #[test]
    fn cmd_send_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-gone".into(),
        ))]);
        let result = cmd_send(&ssh, "gone", "hello", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("gone")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_send_pane_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find pane: skulk-missing".into(),
        ))]);
        let result = cmd_send(&ssh, "missing", "hello", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("missing")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_send_verification_ssh_failure_does_not_false_positive() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("old pane content".into()),
            Ok(String::new()),
            Err(SkulkError::SshFailed("connection lost".into())),
        ]);
        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg).is_ok());
    }
}
