use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent::Agent;
use crate::agent_ref::AgentRef;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::SkulkError;
use crate::inventory::fetch_inventory;
use crate::remote;
use crate::ssh::Ssh;

/// Build the JSON result value for a wait operation.
///
/// On success (`success = true`): `{"ok": true, "agent": "<name>", "status": "idle"}`.
/// On timeout (`success = false`, `reason = Some("timeout")`):
/// `{"ok": false, "agent": "<name>", "reason": "timeout"}`.
pub(crate) fn json_wait_result(name: &str, success: bool, reason: Option<&str>) -> Value {
    if success {
        json!({"ok": true, "agent": name, "status": "idle"})
    } else {
        json!({"ok": false, "agent": name, "reason": reason.unwrap_or("unknown")})
    }
}

/// Build the SSH command to read an agent's idle-marker file.
///
/// The marker file is maintained by the Stop and `UserPromptSubmit` hooks
/// installed at agent creation (see `remote::hooks_settings_json`).
/// It contains `idle`, `busy`, or `pending` (skulk is delivering a prompt;
/// read as busy). If the file doesn't exist yet (the agent has never
/// processed a turn) the command prints `missing`.
pub(crate) fn wait_state_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!(
        "cat ~/.skulk/state/{} 2>/dev/null || echo missing",
        agent.session_name()
    )
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
/// at least one poll. If the agent stays busy past the deadline it returns a
/// `Diagnostic`, or with `--json` prints the timeout payload to stdout and
/// returns `CommandExit(1)`.
pub(crate) fn cmd_wait(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    match wait_for_agent(ssh, &agent, cfg, poll_interval, timeout)? {
        WaitOutcome::Idle => Ok(()),
        WaitOutcome::TimedOut => Err(timeout_error(&[agent], cfg, timeout)),
    }
}

/// How waiting for one agent ended, short of an error.
#[derive(Debug, PartialEq, Eq)]
enum WaitOutcome {
    Idle,
    TimedOut,
}

/// The error a wait returns when `timed_out` (never empty) did not become
/// idle in time.
///
/// With `--json` the timeout is the one failure reported on stdout (printed
/// by [`wait_for_agent`]), so the error carries only the exit code and io.rs
/// adds no second JSON object on stderr.
fn timeout_error(timed_out: &[Agent], cfg: &Config, timeout: Duration) -> SkulkError {
    if cfg.output_format == OutputFormat::Json {
        return SkulkError::CommandExit(1);
    }
    let sessions: Vec<String> = timed_out.iter().map(Agent::session_name).collect();
    let suggestion = match timed_out {
        [agent] => format!(
            "Inspect the agent: `skulk connect {}` (or raise --timeout).",
            agent.name()
        ),
        _ => "Inspect each agent with `skulk connect <name>` (or raise --timeout).".to_string(),
    };
    SkulkError::Diagnostic {
        message: format!(
            "Timed out after {}s waiting for {} to become idle.",
            timeout.as_secs(),
            sessions.join(", ")
        ),
        suggestion,
    }
}

/// The wait loop for an already-resolved agent. Prints the agent's result
/// (idle, or with `--json` the timeout payload) and reports how it ended.
fn wait_for_agent(
    ssh: &impl Ssh,
    agent: &Agent,
    cfg: &Config,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<WaitOutcome, SkulkError> {
    let name = agent.name();

    remote::ensure_session_exists(ssh, name, cfg).map_err(|e| agent.classify(e))?;

    let start = Instant::now();
    loop {
        let state = ssh
            .run(&wait_state_command(name, cfg))
            .map_err(|e| agent.classify(e))?;
        let trimmed = state.trim();
        if trimmed == "idle" || trimmed == "missing" {
            if cfg.output_format == OutputFormat::Json {
                emit_json(&json_wait_result(name, true, None))?;
            } else {
                eprintln!("Agent {} is idle.", agent.session_name());
            }
            return Ok(WaitOutcome::Idle);
        }
        if start.elapsed() >= timeout {
            if cfg.output_format == OutputFormat::Json {
                emit_json(&json_wait_result(name, false, Some("timeout")))?;
            }
            return Ok(WaitOutcome::TimedOut);
        }
        std::thread::sleep(poll_interval);
    }
}

/// Block until every running agent on the host is idle.
///
/// Walks the inventory once, then waits for each session in turn. A host
/// with no running agents is a no-op (just logs a message). `timeout`
/// applies per agent, not in aggregate. A timed-out agent does not stop the
/// walk: every agent gets its result, and the command fails at the end,
/// naming each agent that timed out, if any did.
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
    let mut timed_out = Vec::new();
    for session in &inv.sessions {
        let agent = Agent::from_qualified(session, cfg);
        if wait_for_agent(ssh, &agent, cfg, poll_interval, timeout)? == WaitOutcome::TimedOut {
            timed_out.push(agent);
        }
    }
    if timed_out.is_empty() {
        Ok(())
    } else {
        Err(timeout_error(&timed_out, cfg, timeout))
    }
}

/// Dispatch `skulk wait` between a single agent and `--all`.
///
/// clap requires a name unless `--all` is given, so a missing name reaches
/// `cmd_wait` only for callers that bypass clap; it fails there with the
/// empty-name validation error.
pub(crate) fn run_wait(
    ssh: &impl Ssh,
    name: Option<&str>,
    all: bool,
    cfg: &Config,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<(), SkulkError> {
    if all {
        cmd_wait_all(ssh, cfg, poll_interval, timeout)
    } else {
        cmd_wait(ssh, name.unwrap_or_default(), cfg, poll_interval, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputFormat;
    use crate::testutil::{
        MockSsh, assert_err, mock_empty_inventory, mock_inventory, ssh_ok, test_config,
        test_config_with_format,
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
    fn cmd_wait_keeps_polling_while_prompt_is_pending() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            ssh_ok(),
            Ok("pending".into()),
            Ok("busy".into()),
            Ok("idle".into()),
        ]);
        assert!(cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::from_secs(60)).is_ok());
        assert_eq!(ssh.calls().len(), 4);
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

    #[test]
    fn wait_json_result_idle_has_ok_true() {
        let v = json_wait_result("my-agent", true, None);
        assert_eq!(v["ok"], true);
        assert_eq!(v["agent"], "my-agent");
        assert_eq!(v["status"], "idle");
        assert!(v.get("reason").is_none());
    }

    #[test]
    fn wait_json_result_timeout_has_ok_false() {
        let v = json_wait_result("my-agent", false, Some("timeout"));
        assert_eq!(v["ok"], false);
        assert_eq!(v["agent"], "my-agent");
        assert_eq!(v["reason"], "timeout");
        assert!(v.get("status").is_none());
    }

    #[test]
    fn wait_human_mode_unchanged() {
        let cfg = test_config_with_format(OutputFormat::Human);
        let ssh = MockSsh::new(vec![ssh_ok(), Ok("idle".into())]);
        let result = cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::from_secs(60));
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_wait_human_timeout_returns_diagnostic_err() {
        // Human mode returns Err(Diagnostic) on timeout so the dispatcher
        // prints a human-readable error and exits 1.
        let cfg = test_config_with_format(OutputFormat::Human);
        let ssh = MockSsh::new(vec![
            ssh_ok(),          // has-session
            Ok("busy".into()), // first poll, then timeout fires
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
    fn cmd_wait_json_timeout_returns_command_exit_1() {
        // The timeout payload goes to stdout; the error carries only the exit
        // code so io.rs exits 1 without printing a second JSON object.
        let cfg = test_config_with_format(OutputFormat::Json);
        let ssh = MockSsh::new(vec![
            ssh_ok(),          // has-session
            Ok("busy".into()), // first poll, then timeout fires
        ]);

        let result = cmd_wait(&ssh, "test", &cfg, Duration::ZERO, Duration::ZERO);

        assert_err!(result, SkulkError::CommandExit(1) => {});
    }

    #[test]
    fn cmd_wait_all_human_waits_for_every_agent_after_a_timeout() {
        let cfg = test_config_with_format(OutputFormat::Human);
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-alpha", "skulk-beta"], &[], &[])),
            ssh_ok(),          // has-session alpha
            Ok("busy".into()), // alpha busy, times out
            ssh_ok(),          // has-session beta
            Ok("idle".into()), // beta idle
        ]);

        let result = cmd_wait_all(&ssh, &cfg, Duration::ZERO, Duration::ZERO);

        assert_eq!(ssh.calls().len(), 5, "beta must still be polled");
        assert_err!(result, SkulkError::Diagnostic { message, suggestion } => {
            assert_eq!(
                message,
                "Timed out after 0s waiting for skulk-alpha to become idle."
            );
            assert!(suggestion.contains("skulk connect alpha"), "got: {suggestion}");
        });
    }

    #[test]
    fn cmd_wait_all_human_names_every_agent_that_timed_out() {
        let cfg = test_config_with_format(OutputFormat::Human);
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-alpha", "skulk-beta"], &[], &[])),
            ssh_ok(),          // has-session alpha
            Ok("busy".into()), // alpha times out
            ssh_ok(),          // has-session beta
            Ok("busy".into()), // beta times out
        ]);

        let result = cmd_wait_all(&ssh, &cfg, Duration::ZERO, Duration::ZERO);

        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert_eq!(
                message,
                "Timed out after 0s waiting for skulk-alpha, skulk-beta to become idle."
            );
        });
    }

    #[test]
    fn cmd_wait_all_json_waits_for_every_agent_after_a_timeout() {
        let cfg = test_config_with_format(OutputFormat::Json);
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-alpha", "skulk-beta"], &[], &[])),
            ssh_ok(),          // has-session alpha
            Ok("busy".into()), // alpha busy, times out
            ssh_ok(),          // has-session beta
            Ok("idle".into()), // beta idle
        ]);

        let result = cmd_wait_all(&ssh, &cfg, Duration::ZERO, Duration::ZERO);

        assert_err!(result, SkulkError::CommandExit(1) => {});
        assert_eq!(ssh.calls().len(), 5, "beta must still be polled");
    }

    #[test]
    fn cmd_wait_all_json_stops_at_an_error_that_is_not_a_timeout() {
        let cfg = test_config_with_format(OutputFormat::Json);
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-alpha", "skulk-beta"], &[], &[])),
            ssh_ok(),          // has-session alpha
            Ok("busy".into()), // alpha busy, times out
            Err(SkulkError::SshFailed(
                "can't find session: skulk-beta".into(),
            )),
        ]);

        let result = cmd_wait_all(&ssh, &cfg, Duration::ZERO, Duration::ZERO);

        assert_err!(result, SkulkError::NotFound(_) => {});
    }

    #[test]
    fn run_wait_with_name_waits_for_that_agent() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok(), Ok("idle".into())]);
        let result = run_wait(
            &ssh,
            Some("test"),
            false,
            &cfg,
            Duration::ZERO,
            Duration::from_secs(5),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn run_wait_with_all_walks_the_inventory() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let result = run_wait(
            &ssh,
            None,
            true,
            &cfg,
            Duration::ZERO,
            Duration::from_secs(5),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn run_wait_without_name_or_all_fails_validation_before_ssh() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = run_wait(
            &ssh,
            None,
            false,
            &cfg,
            Duration::ZERO,
            Duration::from_secs(5),
        );
        assert_err!(result, SkulkError::Validation(_) => {});
        assert_eq!(ssh.calls().len(), 0);
    }
}
