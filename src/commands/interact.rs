use std::time::Duration;

use crate::agent_ref::AgentRef;
use crate::commands::destroy::agent_destroy_session_command;
use crate::commands::wait::{has_session_command, mark_busy_command};
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::ssh::Ssh;
use crate::util::{shell_escape, validate_name};

/// Output format for `skulk diff`, mapped from the mutually-exclusive CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffFormat {
    /// Standard unified diff -- `git diff`.
    Default,
    /// Summary of files changed, insertions, deletions -- `git diff --stat`.
    Stat,
    /// Changed file paths only -- `git diff --name-only`.
    NameOnly,
}

/// Build the SSH command to attach to an agent's live tmux session.
pub(crate) fn connect_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("tmux attach-session -t {}", agent.session_name())
}

/// Build the SSH command to detach all clients from an agent's tmux session.
pub(crate) fn disconnect_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("tmux detach-client -s {}", agent.session_name())
}

/// Build the SSH command to diff an agent's branch against the default branch.
pub(crate) fn diff_command(name: &str, format: DiffFormat, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let default_branch = &cfg.default_branch;
    let agent = AgentRef::new(name, cfg);
    let extra = match format {
        DiffFormat::Default => "",
        DiffFormat::Stat => " --stat",
        DiffFormat::NameOnly => " --name-only",
    };
    format!(
        "cd {base_path} && git diff{extra} {default_branch}...{}",
        agent.branch_name()
    )
}

/// Build the SSH command to push an agent's branch to `origin` with upstream tracking.
pub(crate) fn push_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let agent = AgentRef::new(name, cfg);
    format!(
        "cd {base_path} && git push -u origin {}",
        agent.branch_name()
    )
}

/// Build the SSH command to show `git log` of an agent's branch against the default branch.
pub(crate) fn git_log_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let default_branch = &cfg.default_branch;
    let agent = AgentRef::new(name, cfg);
    format!(
        "cd {base_path} && git log {default_branch}..{} --oneline",
        agent.branch_name()
    )
}

/// Build the SSH command to capture the visible pane content of an agent's tmux session.
///
/// Used by both `logs` (snapshot mode) and `send` (delivery verification).
pub(crate) fn capture_pane_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("tmux capture-pane -p -t {}", agent.session_name())
}

/// Build the SSH command to capture N lines of scrollback from an agent's tmux session.
pub(crate) fn logs_snapshot_deep_command(name: &str, lines: u32, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!(
        "tmux capture-pane -p -t {} -S -{lines}",
        agent.session_name()
    )
}

/// Build the SSH command to archive an agent -- kill its tmux session only.
///
/// Archive is the full operation: the worktree and branch are left intact on
/// purpose, so the agent's work can be reviewed or resumed later. Delegates to
/// [`agent_destroy_session_command`] because the shell command is identical;
/// the semantic distinction ("archive" vs "destroy") lives at the `cmd_*`
/// orchestration layer, not in the single-session kill command.
pub(crate) fn archive_command(name: &str, cfg: &Config) -> String {
    agent_destroy_session_command(name, cfg)
}

/// Build the SSH command to send a prompt to a running agent (no startup delay).
///
/// Unlike `agent_send_prompt_command()` (in new.rs) which includes a startup delay,
/// this targets an already-running agent -- no delay needed.
///
/// The chain starts by writing `busy` to the idle marker (see
/// [`mark_busy_command`]) so a subsequent `skulk wait` can't race past a
/// stale `idle` marker while the agent's own `UserPromptSubmit` hook is
/// still propagating. Then splits the actual send into two `tmux send-keys`
/// calls with a short sleep in between: the first types the prompt text,
/// the second submits with Enter. The gap defeats Claude Code's
/// paste-detection, which otherwise swallows the trailing Enter as a
/// newline inside the input box instead of submitting.
pub(crate) fn send_command(name: &str, prompt: &str, cfg: &Config) -> String {
    let escaped = shell_escape(prompt);
    let session_name = AgentRef::new(name, cfg).session_name();
    let mark_busy = mark_busy_command(&session_name);
    format!(
        "{mark_busy} && \
         tmux send-keys -t {session_name} '{escaped}' && \
         sleep 0.1 && \
         tmux send-keys -t {session_name} Enter"
    )
}

/// Show `git diff` between the default branch and an agent's branch.
pub(crate) fn cmd_diff(
    ssh: &impl Ssh,
    name: &str,
    format: DiffFormat,
    cfg: &Config,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    let output = ssh
        .run(&diff_command(name, format, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    print!("{output}");
    Ok(())
}

/// Push an agent's branch to `origin` with upstream tracking.
pub(crate) fn cmd_push(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    let output = ssh
        .run(&push_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    if !output.is_empty() {
        print!("{output}");
    }
    eprintln!(
        "Pushed {} to origin.",
        AgentRef::new(name, cfg).branch_name()
    );
    Ok(())
}

/// Show `git log` of commits on an agent's branch not in the default branch.
pub(crate) fn cmd_git_log(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    let output = ssh
        .run(&git_log_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    print!("{output}");
    Ok(())
}

/// Detach all clients currently attached to an agent's tmux session.
///
/// Useful when an agent was attached from another terminal (or a stuck SSH
/// session) and the local `Ctrl+B D` keybinding is unavailable. The agent
/// keeps running -- only the attached clients are kicked off.
pub(crate) fn cmd_disconnect(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    ssh.run(&disconnect_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    eprintln!(
        "Detached all clients from {}.",
        AgentRef::new(name, cfg).session_name()
    );
    Ok(())
}

/// Attach to an agent's live tmux session via interactive SSH.
pub(crate) fn cmd_connect(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    let session_name = AgentRef::new(name, cfg).session_name();
    // Pre-check that the session exists before launching interactive SSH,
    // because ssh_interactive replaces the process and tmux errors don't
    // propagate as non-zero exit codes in non-interactive contexts.
    ssh.run(&has_session_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    let cmd = connect_command(name, cfg);
    let status = ssh.interactive(&cmd)?;
    if status.success() {
        eprintln!("Detached from {session_name}.");
    } else {
        eprintln!("Connection to {session_name} ended (non-zero exit).");
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

/// Archive an agent: kill its tmux session but leave the worktree and branch.
///
/// Non-destructive counterpart to `cmd_destroy`. Use this to stop an agent
/// whose work you want to review or resume later without losing anything.
pub(crate) fn cmd_archive(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    ssh.run(&archive_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    eprintln!("Archived agent '{name}'. Worktree and branch preserved.");
    Ok(())
}

/// Maximum scrollback lines captured by `skulk transcript`.
///
/// tmux caps the request at its configured `history-limit` (default 2000),
/// so asking for a large number just means "give me everything you have".
/// 100k is well above any realistic per-pane history-limit.
pub(crate) const TRANSCRIPT_MAX_LINES: u32 = 100_000;

/// Dump an agent's full tmux scrollback, to stdout or to a file.
///
/// Captures up to [`TRANSCRIPT_MAX_LINES`] lines of scrollback in a single
/// SSH round-trip. When `output` is `Some`, writes the captured content to
/// that path; otherwise prints to stdout.
///
/// # Errors
///
/// Returns `SkulkError::Validation` if the name is invalid or the output
/// file cannot be written. Returns `SkulkError::NotFound` when the agent's
/// tmux session does not exist. Propagates other SSH errors via
/// [`classify_agent_error`].
pub(crate) fn cmd_transcript(
    ssh: &impl Ssh,
    name: &str,
    output: Option<&std::path::Path>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    let content = ssh
        .run(&logs_snapshot_deep_command(name, TRANSCRIPT_MAX_LINES, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    match output {
        Some(path) => {
            std::fs::write(path, &content).map_err(|e| {
                SkulkError::Validation(format!("Failed to write {}: {e}", path.display()))
            })?;
            eprintln!("Wrote transcript to {}.", path.display());
        }
        None => print!("{content}"),
    }
    Ok(())
}

/// Send a prompt to a running agent with delivery verification.
///
/// `verify_delay` controls how long to wait before checking pane content changed.
/// Production callers should use 500ms; tests can pass `Duration::ZERO`.
pub(crate) fn cmd_send(
    ssh: &impl Ssh,
    name: &str,
    prompt: &str,
    cfg: &Config,
    verify_delay: Duration,
) -> Result<(), SkulkError> {
    validate_name(name)?;
    let session_name = AgentRef::new(name, cfg).session_name();
    let host = &cfg.host;
    let before = ssh
        .run(&capture_pane_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;
    ssh.run(&send_command(name, prompt, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;
    std::thread::sleep(verify_delay);
    match ssh.run(&capture_pane_command(name, cfg)) {
        Ok(after) if after == before => {
            eprintln!(
                "Warning: Prompt sent to {session_name} but delivery could not be confirmed."
            );
        }
        Ok(_) => {
            eprintln!("Prompt delivered to {session_name}.");
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
    fn diff_command_generates_git_diff() {
        let cfg = test_config();
        let cmd = diff_command("my-task", DiffFormat::Default, &cfg);
        assert_eq!(cmd, "cd ~/test-project && git diff main...skulk-my-task");
    }

    #[test]
    fn diff_command_uses_default_branch() {
        let mut cfg = test_config();
        cfg.default_branch = "develop".to_string();
        let cmd = diff_command("my-task", DiffFormat::Default, &cfg);
        assert!(cmd.contains("develop...skulk-my-task"));
    }

    #[test]
    fn diff_command_uses_session_prefix() {
        let cfg = test_config();
        let cmd = diff_command("feat", DiffFormat::Default, &cfg);
        assert!(cmd.contains(&format!("{}feat", cfg.session_prefix)));
    }

    #[test]
    fn diff_command_uses_base_path() {
        let cfg = test_config();
        let cmd = diff_command("feat", DiffFormat::Default, &cfg);
        assert!(cmd.starts_with(&format!("cd {}", cfg.base_path)));
    }

    #[test]
    fn diff_command_default_has_no_format_flag() {
        let cfg = test_config();
        let cmd = diff_command("my-task", DiffFormat::Default, &cfg);
        assert!(!cmd.contains("--stat"));
        assert!(!cmd.contains("--name-only"));
    }

    #[test]
    fn diff_command_stat_appends_stat_flag() {
        let cfg = test_config();
        let cmd = diff_command("my-task", DiffFormat::Stat, &cfg);
        assert_eq!(
            cmd,
            "cd ~/test-project && git diff --stat main...skulk-my-task"
        );
    }

    #[test]
    fn diff_command_name_only_appends_name_only_flag() {
        let cfg = test_config();
        let cmd = diff_command("my-task", DiffFormat::NameOnly, &cfg);
        assert_eq!(
            cmd,
            "cd ~/test-project && git diff --name-only main...skulk-my-task"
        );
    }

    #[test]
    fn cmd_diff_succeeds_and_prints_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("diff --git a/foo b/foo\n+hello".into())]);
        assert!(cmd_diff(&ssh, "test", DiffFormat::Default, &cfg).is_ok());
    }

    #[test]
    fn cmd_diff_returns_empty_output_when_no_changes() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        assert!(cmd_diff(&ssh, "test", DiffFormat::Default, &cfg).is_ok());
    }

    #[test]
    fn cmd_diff_stat_succeeds_and_prints_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(" foo.rs | 2 +-\n 1 file changed".into())]);
        assert!(cmd_diff(&ssh, "test", DiffFormat::Stat, &cfg).is_ok());
    }

    #[test]
    fn cmd_diff_name_only_succeeds_and_prints_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("foo.rs\nbar.rs".into())]);
        assert!(cmd_diff(&ssh, "test", DiffFormat::NameOnly, &cfg).is_ok());
    }

    #[test]
    fn cmd_diff_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "fatal: ambiguous argument 'main...skulk-nope': unknown revision or path not in the working tree".into(),
        ))]);
        let result = cmd_diff(&ssh, "nope", DiffFormat::Default, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("nope")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_diff_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_diff(&ssh, "../bad", DiffFormat::Default, &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn push_command_generates_git_push_with_upstream() {
        let cfg = test_config();
        let cmd = push_command("my-task", &cfg);
        assert_eq!(cmd, "cd ~/test-project && git push -u origin skulk-my-task");
    }

    #[test]
    fn push_command_uses_session_prefix() {
        let cfg = test_config();
        let cmd = push_command("feat", &cfg);
        assert!(cmd.contains(&format!("{}feat", cfg.session_prefix)));
    }

    #[test]
    fn push_command_uses_base_path() {
        let cfg = test_config();
        let cmd = push_command("feat", &cfg);
        assert!(cmd.starts_with(&format!("cd {}", cfg.base_path)));
    }

    #[test]
    fn push_command_sets_upstream() {
        let cfg = test_config();
        let cmd = push_command("feat", &cfg);
        assert!(cmd.contains("-u origin"));
    }

    #[test]
    fn cmd_push_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        assert!(cmd_push(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_push_succeeds_with_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(
            "branch skulk-test set up to track origin/skulk-test".into(),
        )]);
        assert!(cmd_push(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_push_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_push(&ssh, "../bad", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn cmd_push_generic_failure_surfaces_ssh_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "error: failed to push some refs".into(),
        ))]);
        let result = cmd_push(&ssh, "test", &cfg);
        assert!(matches!(result, Err(SkulkError::SshFailed(_))));
    }

    #[test]
    fn cmd_push_branch_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "error: src refspec skulk-nope does not match any".into(),
        ))]);
        let result = cmd_push(&ssh, "nope", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("nope")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_push_no_origin_remote() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "fatal: 'origin' does not appear to be a git repository\nfatal: Could not read from remote repository.".into(),
        ))]);
        let result = cmd_push(&ssh, "test", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Diagnostic {
                message,
                suggestion,
            } => {
                assert!(message.to_lowercase().contains("origin"));
                assert!(!suggestion.is_empty());
            }
            other => panic!("expected Diagnostic, got: {other}"),
        }
    }

    #[test]
    fn disconnect_command_generates_tmux_detach_client() {
        let cfg = test_config();
        let cmd = disconnect_command("my-task", &cfg);
        assert_eq!(cmd, "tmux detach-client -s skulk-my-task");
    }

    #[test]
    fn disconnect_command_uses_session_prefix() {
        let cfg = test_config();
        let cmd = disconnect_command("test", &cfg);
        assert!(cmd.contains(&*cfg.session_prefix));
    }

    #[test]
    fn cmd_disconnect_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        assert!(cmd_disconnect(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_disconnect_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_disconnect(&ssh, "ghost", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_disconnect_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_disconnect(&ssh, "../bad", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
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
    fn send_command_types_prompt_then_submits() {
        let cfg = test_config();
        let cmd = send_command("my-task", "fix the bug", &cfg);
        assert!(cmd.contains("tmux send-keys -t skulk-my-task 'fix the bug'"));
        assert!(cmd.contains("tmux send-keys -t skulk-my-task Enter"));
    }

    #[test]
    fn send_command_marks_busy_before_typing() {
        let cfg = test_config();
        let cmd = send_command("my-task", "fix the bug", &cfg);
        let busy_idx = cmd.find("printf busy").expect("busy marker write missing");
        let type_idx = cmd
            .find("'fix the bug'")
            .expect("prompt typing step missing");
        assert!(
            busy_idx < type_idx,
            "busy marker must be written before send-keys: {cmd}"
        );
    }

    #[test]
    fn send_command_splits_typing_and_submit_with_sleep() {
        let cfg = test_config();
        let cmd = send_command("my-task", "fix the bug", &cfg);
        let type_idx = cmd
            .find("'fix the bug'")
            .expect("prompt typing step missing");
        let sleep_idx = cmd.find("sleep").expect("submit delay missing");
        let enter_idx = cmd.find("Enter").expect("submit step missing");
        assert!(type_idx < sleep_idx, "sleep must come after typing prompt");
        assert!(sleep_idx < enter_idx, "Enter must come after sleep");
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
        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_send_succeeds_with_unconfirmed_delivery() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("same pane content".into()),
            Ok(String::new()),
            Ok("same pane content".into()),
        ]);
        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn cmd_send_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-gone".into(),
        ))]);
        let result = cmd_send(&ssh, "gone", "hello", &cfg, Duration::ZERO);
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
        let result = cmd_send(&ssh, "missing", "hello", &cfg, Duration::ZERO);
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
        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg, Duration::ZERO).is_ok());
    }

    #[test]
    fn archive_command_generates_tmux_kill_session() {
        let cfg = test_config();
        let cmd = archive_command("my-task", &cfg);
        assert_eq!(cmd, "tmux kill-session -t skulk-my-task");
    }

    #[test]
    fn archive_command_uses_session_prefix() {
        let cfg = test_config();
        let cmd = archive_command("test", &cfg);
        assert!(cmd.contains(&*cfg.session_prefix));
    }

    #[test]
    fn archive_command_does_not_touch_worktree_or_branch() {
        let cfg = test_config();
        let cmd = archive_command("my-task", &cfg);
        assert!(!cmd.contains("worktree"));
        assert!(!cmd.contains("branch"));
        assert!(!cmd.contains("git"));
    }

    #[test]
    fn cmd_archive_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        assert!(cmd_archive(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_archive_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_archive(&ssh, "ghost", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_archive_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_archive(&ssh, "../bad", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn git_log_command_generates_git_log() {
        let cfg = test_config();
        let cmd = git_log_command("my-task", &cfg);
        assert_eq!(
            cmd,
            "cd ~/test-project && git log main..skulk-my-task --oneline"
        );
    }

    #[test]
    fn git_log_command_uses_default_branch() {
        let mut cfg = test_config();
        cfg.default_branch = "develop".to_string();
        let cmd = git_log_command("my-task", &cfg);
        assert!(cmd.contains("develop..skulk-my-task"));
    }

    #[test]
    fn git_log_command_uses_session_prefix() {
        let cfg = test_config();
        let cmd = git_log_command("feat", &cfg);
        assert!(cmd.contains(&format!("{}feat", cfg.session_prefix)));
    }

    #[test]
    fn git_log_command_uses_base_path() {
        let cfg = test_config();
        let cmd = git_log_command("feat", &cfg);
        assert!(cmd.starts_with(&format!("cd {}", cfg.base_path)));
    }

    #[test]
    fn git_log_command_uses_two_dot_range() {
        let cfg = test_config();
        let cmd = git_log_command("feat", &cfg);
        assert!(cmd.contains("main..skulk-feat"));
        assert!(!cmd.contains("main...skulk-feat"));
    }

    #[test]
    fn git_log_command_includes_oneline_flag() {
        let cfg = test_config();
        let cmd = git_log_command("feat", &cfg);
        assert!(cmd.contains("--oneline"));
    }

    #[test]
    fn cmd_git_log_succeeds_and_prints_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("abc1234 first commit\ndef5678 second".into())]);
        assert!(cmd_git_log(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_git_log_returns_empty_output_when_no_commits() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        assert!(cmd_git_log(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_git_log_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "fatal: ambiguous argument 'main..skulk-nope': unknown revision or path not in the working tree".into(),
        ))]);
        let result = cmd_git_log(&ssh, "nope", &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("nope")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_git_log_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_git_log(&ssh, "../bad", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn cmd_transcript_prints_to_stdout_when_no_output_path() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("scrollback line 1\nscrollback line 2".into())]);
        assert!(cmd_transcript(&ssh, "test", None, &cfg).is_ok());
    }

    #[test]
    fn cmd_transcript_writes_to_file_when_output_path() {
        let cfg = test_config();
        let expected = "scrollback line 1\nscrollback line 2\n";
        let ssh = MockSsh::new(vec![Ok(expected.into())]);
        let path =
            std::env::temp_dir().join(format!("skulk-transcript-test-{}.txt", std::process::id()));
        // Ensure no leftover from a previous run.
        let _ = std::fs::remove_file(&path);

        let result = cmd_transcript(&ssh, "test", Some(&path), &cfg);

        // Read-then-delete-then-assert so cleanup runs even if the file
        // contents don't match expectations.
        let written = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);

        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert_eq!(written.expect("transcript file should exist"), expected);
    }

    #[test]
    fn cmd_transcript_returns_validation_error_when_write_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("scrollback content".into())]);
        // Non-existent directory makes fs::write fail.
        let path =
            std::path::PathBuf::from("/nonexistent-skulk-dir-that-should-not-exist/transcript.txt");
        let result = cmd_transcript(&ssh, "test", Some(&path), &cfg);
        match result {
            Err(SkulkError::Validation(msg)) => {
                assert!(msg.contains("Failed to write"), "unexpected message: {msg}");
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn cmd_transcript_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_transcript(&ssh, "ghost", None, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_transcript_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_transcript(&ssh, "../bad", None, &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }
}
