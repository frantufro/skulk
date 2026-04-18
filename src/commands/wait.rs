use std::time::Duration;

use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::{inventory_command, parse_inventory};
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command to read an agent's idle-marker file.
///
/// The marker file is maintained by the Stop and `UserPromptSubmit` hooks
/// installed at agent creation (see `commands::new::hooks_settings_json`).
/// It contains either `idle` or `busy`. If the file doesn't exist yet
/// (the agent has never processed a turn) the command prints `missing`.
pub(crate) fn wait_state_command(name: &str, cfg: &Config) -> String {
    let session_prefix = &cfg.session_prefix;
    format!("cat ~/.skulk/state/{session_prefix}{name} 2>/dev/null || echo missing")
}

/// Build the SSH command used to confirm the agent's tmux session exists.
fn has_session_command(name: &str, cfg: &Config) -> String {
    let session_prefix = &cfg.session_prefix;
    format!("tmux has-session -t {session_prefix}{name}")
}

/// Block until the named agent is idle (finished its current turn).
///
/// Verifies the tmux session exists up front, then polls the idle marker
/// until it reports `idle`. A missing marker is treated as idle — the agent
/// has not yet processed any prompt, so there is nothing to wait for.
///
/// `poll_interval` is exposed for testing; production callers should pass
/// [`crate::WAIT_POLL_INTERVAL`].
pub(crate) fn cmd_wait(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
    poll_interval: Duration,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    let session_prefix = &cfg.session_prefix;
    let host = &cfg.host;

    ssh.run(&has_session_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;

    loop {
        let state = ssh
            .run(&wait_state_command(name, cfg))
            .map_err(|e| classify_agent_error(name, e, host))?;
        let trimmed = state.trim();
        if trimmed == "idle" || trimmed == "missing" {
            eprintln!("Agent {session_prefix}{name} is idle.");
            return Ok(());
        }
        std::thread::sleep(poll_interval);
    }
}

/// Block until every running agent on the host is idle.
///
/// Walks the inventory once, then calls [`cmd_wait`] for each session in
/// turn. A host with no running agents is a no-op (just logs a message).
pub(crate) fn cmd_wait_all(
    ssh: &impl Ssh,
    cfg: &Config,
    poll_interval: Duration,
) -> Result<(), SkulkError> {
    let inv = parse_inventory(&ssh.run(&inventory_command(cfg))?, cfg);
    if inv.sessions.is_empty() {
        eprintln!("No running agents.");
        return Ok(());
    }
    for session in &inv.sessions {
        let name = session
            .strip_prefix(&*cfg.session_prefix)
            .unwrap_or(session);
        cmd_wait(ssh, name, cfg, poll_interval)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, mock_inventory, test_config};

    #[test]
    fn wait_state_command_reads_marker_with_fallback() {
        let cfg = test_config();
        let cmd = wait_state_command("my-task", &cfg);
        assert_eq!(
            cmd,
            "cat ~/.skulk/state/skulk-my-task 2>/dev/null || echo missing"
        );
    }

    #[test]
    fn wait_state_command_uses_session_prefix() {
        let cfg = test_config();
        let cmd = wait_state_command("feat", &cfg);
        assert!(cmd.contains(&format!("{}feat", cfg.session_prefix)));
    }

    #[test]
    fn has_session_command_generates_tmux_probe() {
        let cfg = test_config();
        let cmd = has_session_command("my-task", &cfg);
        assert_eq!(cmd, "tmux has-session -t skulk-my-task");
    }

    #[test]
    fn cmd_wait_returns_immediately_when_idle() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()), // has-session
            Ok("idle".into()), // first poll
        ]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_wait_polls_until_idle() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()),
            Ok("busy".into()),
            Ok("busy".into()),
            Ok("idle".into()),
        ]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO).is_ok());
        assert_eq!(ssh.calls().len(), 4);
    }

    #[test]
    fn cmd_wait_treats_missing_marker_as_idle() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new()), Ok("missing".into())]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_wait_trims_trailing_whitespace_before_matching() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new()), Ok("idle\n".into())]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_wait_returns_not_found_when_session_missing() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_wait(&ssh, "ghost", &cfg, Duration::ZERO);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_wait_surfaces_poll_ssh_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()),
            Err(SkulkError::SshFailed("connection lost".into())),
        ]);
        let result = cmd_wait(&ssh, "test", &cfg, Duration::ZERO);
        assert!(matches!(result, Err(SkulkError::SshFailed(_))));
    }

    #[test]
    fn cmd_wait_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_wait(&ssh, "../bad", &cfg, Duration::ZERO);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn cmd_wait_all_iterates_each_running_agent() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-alpha", "skulk-beta"], &[], &[])),
            Ok(String::new()), // has-session alpha
            Ok("idle".into()), // alpha idle
            Ok(String::new()), // has-session beta
            Ok("idle".into()), // beta idle
        ]);
        assert!(cmd_wait_all(&ssh, &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_wait_all_no_sessions_is_ok() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(&[], &[], &[]))]);
        assert!(cmd_wait_all(&ssh, &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_wait_all_propagates_first_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-alpha"], &[], &[])),
            Err(SkulkError::SshFailed(
                "can't find session: skulk-alpha".into(),
            )),
        ]);
        let result = cmd_wait_all(&ssh, &cfg, Duration::ZERO);
        assert!(matches!(result, Err(SkulkError::NotFound(_))));
    }
}
