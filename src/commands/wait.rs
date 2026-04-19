use std::time::{Duration, Instant};

use crate::agent_ref::AgentRef;
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command to read an agent's idle-marker file.
///
/// The marker file is maintained by the Stop and `UserPromptSubmit` hooks
/// installed at agent creation (see `commands::new::hooks_settings_json`).
/// It contains either `idle` or `busy`. If the file doesn't exist yet
/// (the agent has never processed a turn) the command prints `missing`.
pub(crate) fn wait_state_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!(
        "cat ~/.skulk/state/{} 2>/dev/null || echo missing",
        agent.session_name()
    )
}

/// Build the SSH command used to confirm the agent's tmux session exists.
pub(crate) fn has_session_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("tmux has-session -t {}", agent.session_name())
}

/// Build a shell snippet that atomically writes `busy` to the idle marker.
///
/// Prepended to `tmux send-keys` chains in `cmd_send` and `cmd_new` so the
/// marker is set before the prompt is delivered. This closes the race where
/// `skulk wait`, invoked immediately after `skulk send`, could observe a
/// stale `idle` (or `missing`) marker and return prematurely — the agent's
/// own `UserPromptSubmit` hook fires asynchronously after Claude Code reads
/// the terminal input, which can be milliseconds to seconds after send-keys.
pub(crate) fn mark_busy_command(session_name: &str) -> String {
    format!("mkdir -p ~/.skulk/state && printf busy > ~/.skulk/state/{session_name}")
}

/// Block until the named agent is idle (finished its current turn).
///
/// Verifies the tmux session exists up front, then polls the idle marker
/// until it reports `idle`. A missing marker is treated as idle — the agent
/// has not yet processed any prompt, so there is nothing to wait for.
///
/// `poll_interval` and `timeout` are exposed for testing; production callers
/// supply them from `Timings` and the CLI flag respectively. The timeout is
/// checked after each poll that sees `busy`, so the function always attempts
/// at least one poll and returns a `Diagnostic` if the agent stays busy
/// past the deadline.
pub(crate) fn cmd_wait(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    let host = &cfg.host;
    let session_name = AgentRef::new(name, cfg).session_name();

    ssh.run(&has_session_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;

    let start = Instant::now();
    loop {
        let state = ssh
            .run(&wait_state_command(name, cfg))
            .map_err(|e| classify_agent_error(name, e, host))?;
        let trimmed = state.trim();
        if trimmed == "idle" || trimmed == "missing" {
            eprintln!("Agent {session_name} is idle.");
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(SkulkError::Diagnostic {
                message: format!(
                    "Timed out after {}s waiting for {session_name} to become idle.",
                    timeout.as_secs()
                ),
                suggestion: format!(
                    "Inspect the agent: `skulk connect {name}` (or raise --timeout)."
                ),
            });
        }
        std::thread::sleep(poll_interval);
    }
}

/// Block until every running agent on the host is idle.
///
/// Walks the inventory once, then calls [`cmd_wait`] for each session in
/// turn. A host with no running agents is a no-op (just logs a message).
/// `timeout` applies per agent, not in aggregate.
pub(crate) fn cmd_wait_all(
    ssh: &impl Ssh,
    cfg: &Config,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<(), SkulkError> {
    let inv = fetch_inventory(ssh, cfg)?;
    if inv.sessions.is_empty() {
        eprintln!("No running agents.");
        return Ok(());
    }
    for session in &inv.sessions {
        let agent = AgentRef::from_qualified(session, cfg);
        cmd_wait(ssh, agent.name(), cfg, poll_interval, timeout)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, mock_empty_inventory, mock_inventory, ssh_ok, test_config,
    };

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
    fn mark_busy_command_writes_busy_to_session_marker() {
        let cmd = mark_busy_command("skulk-my-task");
        assert_eq!(
            cmd,
            "mkdir -p ~/.skulk/state && printf busy > ~/.skulk/state/skulk-my-task"
        );
    }

    #[test]
    fn cmd_wait_returns_immediately_when_idle() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            ssh_ok(),          // has-session
            Ok("idle".into()), // first poll
        ]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn cmd_wait_polls_until_idle() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            ssh_ok(),
            Ok("busy".into()),
            Ok("busy".into()),
            Ok("idle".into()),
        ]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::from_secs(60)).is_ok());
        assert_eq!(ssh.calls().len(), 4);
    }

    #[test]
    fn cmd_wait_times_out_when_agent_stays_busy() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            ssh_ok(),          // has-session
            Ok("busy".into()), // first poll, then timeout check fires
        ]);
        let result = cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::ZERO);
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(
                message.to_lowercase().contains("timed out"),
                "expected timeout message, got: {message}"
            );
        });
    }

    #[test]
    fn cmd_wait_polls_at_least_once_even_with_zero_timeout_when_already_idle() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok(), Ok("idle".into())]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_wait_treats_missing_marker_as_idle() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok(), Ok("missing".into())]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn cmd_wait_trims_trailing_whitespace_before_matching() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok(), Ok("idle\n".into())]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn cmd_wait_returns_not_found_when_session_missing() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_wait(&ssh, "ghost", &cfg, Duration::ZERO, Duration::from_secs(60));
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"));
        });
    }

    #[test]
    fn cmd_wait_surfaces_poll_ssh_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            ssh_ok(),
            Err(SkulkError::SshFailed("connection lost".into())),
        ]);
        let result = cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::from_secs(60));
        assert!(matches!(result, Err(SkulkError::SshFailed(_))));
    }

    #[test]
    fn cmd_wait_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_wait(
            &ssh,
            "../bad",
            &cfg,
            Duration::ZERO,
            Duration::from_secs(60),
        );
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn cmd_wait_all_iterates_each_running_agent() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-alpha", "skulk-beta"], &[], &[])),
            ssh_ok(),          // has-session alpha
            Ok("idle".into()), // alpha idle
            ssh_ok(),          // has-session beta
            Ok("idle".into()), // beta idle
        ]);
        assert!(cmd_wait_all(&ssh, &cfg, Duration::ZERO, Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn cmd_wait_all_no_sessions_is_ok() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        assert!(cmd_wait_all(&ssh, &cfg, Duration::ZERO, Duration::from_secs(60)).is_ok());
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
        let result = cmd_wait_all(&ssh, &cfg, Duration::ZERO, Duration::from_secs(60));
        assert!(matches!(result, Err(SkulkError::NotFound(_))));
    }
}
