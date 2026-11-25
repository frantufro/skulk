use std::path::Path;

use crate::agent::Agent;
use crate::agent_ref::AgentRef;
use crate::commands::prompt_source;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::SkulkError;
use crate::remote::{self, mark_pending_command};
use crate::ssh::Ssh;
use crate::submit::{Submission, SubmitPacing, confirm_submission, settle_delay, sleep_arg};
use crate::util::{shell_escape, shell_quote_path, validate_window_name};

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

impl DiffFormat {
    /// Interpret the `--stat` / `--name-only` flags (clap makes them mutually exclusive).
    pub(crate) fn from_flags(stat: bool, name_only: bool) -> Self {
        match (stat, name_only) {
            (true, _) => DiffFormat::Stat,
            (_, true) => DiffFormat::NameOnly,
            _ => DiffFormat::Default,
        }
    }
}

/// Resolve the text for `skulk send` from its two mutually exclusive sources.
///
/// `--from <file>` is read verbatim (trailing newlines trimmed); otherwise the
/// positional prompt is used. clap requires exactly one of the two, so a
/// missing positional prompt only arises for callers that bypass clap, and
/// yields an empty prompt that `cmd_send` hands to the agent unchanged.
pub(crate) fn resolve_send_prompt(
    prompt: Option<String>,
    from: Option<&Path>,
) -> Result<String, SkulkError> {
    match from {
        Some(path) => prompt_source::load_file_raw(path),
        None => Ok(prompt.unwrap_or_default()),
    }
}

/// Build the SSH command to attach to an agent's live tmux session.
///
/// Attaches to the agent's window (see [`remote::resolve_agent_window`]), not
/// the bare session, so the user always lands on the harness. Attaching to a
/// window target also makes it the session's current window, so a `server`
/// window someone left active is switched away from for every attached client.
///
/// It first hides the status line, so sessions created before skulk did that
/// at creation lose it too. That step is `;`-chained: a cosmetic option must
/// never stop the attach.
pub(crate) fn connect_command(name: &str, cfg: &Config) -> String {
    let session = AgentRef::new(name, cfg).session_name();
    let hide_status = remote::hide_status_line_command(&session);
    let resolve = remote::resolve_agent_window(&session);
    format!("{hide_status}; {resolve} && tmux attach-session -t \"$skulk_win\"")
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

/// Field separator inside a `git log -z` record (ASCII unit separator).
const GIT_LOG_FIELD_SEP: char = '\u{1f}';

/// Number of fields in one `git log` JSON record.
const GIT_LOG_FIELDS: usize = 6;

/// Build the SSH command for `git-log --json`: one NUL-terminated record per
/// commit, fields split by the unit separator. The subject comes last so that
/// [`json_git_log_result`] can keep it whole even if it contains the separator.
pub(crate) fn git_log_json_command(name: &str, limit: u32, cfg: &Config) -> String {
    let base_path = shell_quote_path(&cfg.base_path);
    let range = format!(
        "{}..{}",
        cfg.default_branch,
        AgentRef::new(name, cfg).branch_name()
    );
    format!(
        "cd {base_path} && git log '{}' -n {limit} -z \
         --format='%H%x1f%h%x1f%aI%x1f%cI%x1f%an%x1f%s'",
        shell_escape(&range)
    )
}

/// Parse the output of [`git_log_json_command`] into the `git-log --json` payload.
pub(crate) fn json_git_log_result(
    name: &str,
    cfg: &Config,
    output: &str,
) -> Result<serde_json::Value, SkulkError> {
    let mut commits = Vec::new();
    for record in output.split('\0').map(|r| r.trim_matches('\n')) {
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(GIT_LOG_FIELDS, GIT_LOG_FIELD_SEP).collect();
        let [sha, short, authored_at, committed_at, author, subject] = fields[..] else {
            return Err(SkulkError::SshFailed(format!(
                "Unexpected git log record: {record:?}"
            )));
        };
        commits.push(serde_json::json!({
            "sha": sha,
            "short": short,
            "subject": subject,
            "author": author,
            "authored_at": authored_at,
            "committed_at": committed_at,
        }));
    }
    Ok(serde_json::json!({
        "v": crate::display::JSON_SCHEMA_VERSION,
        "agent": name,
        "base": cfg.default_branch,
        "commits": commits,
    }))
}

/// Build the SSH command to capture N lines of scrollback from an agent's tmux session.
pub(crate) fn logs_snapshot_deep_command(name: &str, lines: u32, cfg: &Config) -> String {
    logs_command(name, None, Some(lines), cfg)
}

/// Build the SSH command behind `skulk logs`: capture the agent's harness
/// window, or the window named `window`, with `lines` of scrollback (visible
/// pane only when `None`). `window` must have passed `validate_window_name`.
pub(crate) fn logs_command(
    name: &str,
    window: Option<&str>,
    lines: Option<u32>,
    cfg: &Config,
) -> String {
    let session = AgentRef::new(name, cfg).session_name();
    let resolve = remote::resolve_window(&session, window);
    let scrollback = lines.map_or_else(String::new, |n| format!(" -S -{n}"));
    format!("{resolve} && tmux capture-pane -p -t \"$skulk_win\"{scrollback}")
}

/// Build the SSH command to write an archive reason sidecar file.
///
/// Stored at `~/.skulk/archive/<session_name>.txt` so callers can later
/// inspect why an agent was archived. `reason` must be the raw, unescaped
/// string; this function applies `shell_escape` internally.
pub(crate) fn archive_reason_command(session_name: &str, reason: &str) -> String {
    format!(
        "mkdir -p ~/.skulk/archive && printf '%s' '{}' > ~/.skulk/archive/{session_name}.txt",
        shell_escape(reason)
    )
}

/// Build the SSH command to send a prompt to a running agent (no startup delay).
///
/// Unlike `agent_send_prompt_command()` (in new.rs) which includes a startup delay,
/// this targets an already-running agent -- no delay needed.
///
/// The chain starts by writing `pending` to the idle marker (see
/// [`mark_pending_command`]) so a subsequent `skulk wait` can't race past a
/// stale `idle` marker while the agent's own `UserPromptSubmit` hook is
/// still propagating. Then splits the actual send into two `tmux send-keys`
/// calls with a sleep in between, longer for a larger prompt (see
/// [`settle_delay`]): the first types the prompt text, the second submits
/// with Enter. The gap defeats Claude Code's paste-detection, which
/// otherwise swallows the trailing Enter as a newline inside the input box
/// instead of submitting.
pub(crate) fn send_command(name: &str, prompt: &str, cfg: &Config) -> String {
    let escaped = shell_escape(prompt);
    let session_name = AgentRef::new(name, cfg).session_name();
    let mark_pending = mark_pending_command(&session_name);
    let resolve = remote::resolve_agent_window(&session_name);
    let settle = sleep_arg(settle_delay(prompt.len()));
    format!(
        "{mark_pending} && {resolve} && \
         tmux send-keys -t \"$skulk_win\" '{escaped}' && \
         sleep {settle} && \
         tmux send-keys -t \"$skulk_win\" Enter"
    )
}

/// Show `git diff` between the default branch and an agent's branch.
pub(crate) fn cmd_diff(
    ssh: &impl Ssh,
    name: &str,
    format: DiffFormat,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    let output = ssh
        .run(&diff_command(name, format, cfg))
        .map_err(|e| agent.classify(e))?;
    match cfg.output_format {
        OutputFormat::Json => emit_json(&serde_json::json!({
            "agent": name,
            "diff": output,
        })),
        OutputFormat::Human => print!("{output}"),
    }
    Ok(())
}

/// Push an agent's branch to `origin` with upstream tracking.
pub(crate) fn cmd_push(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    let output = ssh
        .run(&push_command(name, cfg))
        .map_err(|e| agent.classify(e))?;
    match cfg.output_format {
        OutputFormat::Json => emit_json(&json_push_result(name, cfg, &output)),
        OutputFormat::Human => {
            if !output.is_empty() {
                print!("{output}");
            }
            eprintln!("Pushed {} to origin.", agent.branch_name());
        }
    }
    Ok(())
}

/// Build the JSON result for a successful `push`.
pub(crate) fn json_push_result(name: &str, cfg: &Config, output: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "agent": name,
        "branch": AgentRef::new(name, cfg).branch_name(),
        "remote": "origin",
        "output": output.trim(),
    })
}

/// Show `git log` of commits on an agent's branch not in the default branch.
///
/// `limit` caps the commits in the JSON output only; text output is unchanged.
pub(crate) fn cmd_git_log(
    ssh: &impl Ssh,
    name: &str,
    limit: u32,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    let json = matches!(cfg.output_format, OutputFormat::Json);
    let command = if json {
        git_log_json_command(name, limit, cfg)
    } else {
        git_log_command(name, cfg)
    };
    let output = ssh.run(&command).map_err(|e| agent.classify(e))?;
    if json {
        emit_json(&json_git_log_result(name, cfg, &output)?);
    } else {
        print!("{output}");
    }
    Ok(())
}

/// Detach all clients currently attached to an agent's tmux session.
///
/// Useful when an agent was attached from another terminal (or a stuck SSH
/// session) and the local `Ctrl+B D` keybinding is unavailable. The agent
/// keeps running -- only the attached clients are kicked off.
pub(crate) fn cmd_disconnect(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    ssh.run(&disconnect_command(name, cfg))
        .map_err(|e| agent.classify(e))?;
    eprintln!("Detached all clients from {}.", agent.session_name());
    Ok(())
}

/// Attach to an agent's live tmux session via interactive SSH.
pub(crate) fn cmd_connect(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    let session_name = agent.session_name();
    // Pre-check that the session exists before launching interactive SSH,
    // because ssh_interactive replaces the process and tmux errors don't
    // propagate as non-zero exit codes in non-interactive contexts.
    remote::ensure_session_exists(ssh, name, cfg).map_err(|e| agent.classify(e))?;
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
///
/// Reads the agent's harness window, or the window named `window` (e.g. the
/// `server` window an init hook opens) when given.
pub(crate) fn cmd_logs(
    ssh: &impl Ssh,
    name: &str,
    follow: bool,
    lines: Option<u32>,
    window: Option<&str>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    if let Some(w) = window {
        validate_window_name(w)?;
    }
    if follow {
        return crate::commands::follow::cmd_logs_follow(ssh, &agent, window, cfg);
    }
    let output = ssh
        .run(&logs_command(name, window, lines, cfg))
        .map_err(|e| classify_logs_error(&agent, window, e))?;
    match cfg.output_format {
        OutputFormat::Json => {
            let mut doc = serde_json::json!({
                "agent": name,
                "lines": output.lines().collect::<Vec<_>>(),
            });
            if let Some(w) = window {
                doc["window"] = w.into();
            }
            emit_json(&doc);
        }
        OutputFormat::Human => print!("{output}"),
    }
    Ok(())
}

/// Map a failed capture for `skulk logs` to the user-facing error, naming the
/// window when a specific one was asked for.
pub(crate) fn classify_logs_error(
    agent: &Agent,
    window: Option<&str>,
    err: SkulkError,
) -> SkulkError {
    match window {
        Some(w) => agent.classify_window(w, err),
        None => agent.classify(err),
    }
}

/// Restart one window of a running agent's session, e.g. the `server` window
/// an init hook opened, without touching the harness or re-running init.
///
/// The window is respawned with the command it was created with (see
/// [`remote::respawn_window`]). The harness window is refused: restarting it
/// would kill the agent mid-task — `skulk restart` is the tool for that.
pub(crate) fn cmd_restart_window(
    ssh: &impl Ssh,
    name: &str,
    window: &str,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    validate_window_name(window)?;
    if window == remote::AGENT_WINDOW {
        return Err(agent_window_refusal(name, window));
    }
    remote::respawn_window(ssh, name, window, cfg).map_err(|e| match e {
        SkulkError::SshFailed(stderr) if stderr.contains(remote::AGENT_WINDOW_REFUSAL) => {
            agent_window_refusal(name, window)
        }
        other => agent.classify_window(window, other),
    })?;
    match cfg.output_format {
        OutputFormat::Json => emit_json(&serde_json::json!({
            "agent": name,
            "window": window,
            "restarted": true,
        })),
        OutputFormat::Human => eprintln!("Restarted window '{window}' of agent '{name}'."),
    }
    Ok(())
}

fn agent_window_refusal(name: &str, window: &str) -> SkulkError {
    SkulkError::Validation(format!(
        "Window '{window}' runs the agent itself and can't be restarted this way. \
         Use `skulk archive {name}` and `skulk restart {name}` instead."
    ))
}

/// Archive an agent: kill its tmux session but leave the worktree and branch.
///
/// Non-destructive counterpart to `cmd_destroy`. Use this to stop an agent
/// whose work you want to review or resume later without losing anything.
///
/// The sidecar file `~/.skulk/archive/<session>.txt` is always written: it
/// holds `reason` (empty when none is given), and its mtime is how `skulk
/// events` tells an archived agent from a crashed one. The sidecar write is
/// non-fatal: if it fails, a warning is printed to stderr and `Ok` is still
/// returned.
pub(crate) fn cmd_archive(
    ssh: &impl Ssh,
    name: &str,
    reason: Option<&str>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let warnings = archive_agent(ssh, name, reason, cfg)?;
    match cfg.output_format {
        OutputFormat::Json => emit_json(&json_archive_result(name, reason, &warnings, cfg)),
        OutputFormat::Human => {
            for warning in &warnings {
                eprintln!("Warning: {warning}");
            }
            eprintln!("Archived agent '{name}'. Worktree and branch preserved.");
        }
    }
    Ok(())
}

/// Kill the agent's session and write the archive sidecar, returning
/// non-fatal warnings.
fn archive_agent(
    ssh: &impl Ssh,
    name: &str,
    reason: Option<&str>,
    cfg: &Config,
) -> Result<Vec<String>, SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    remote::kill_session(ssh, name, cfg).map_err(|e| agent.classify(e))?;
    let mut warnings = Vec::new();
    let session_name = agent.session_name();
    if let Err(e) = ssh.run(&archive_reason_command(
        &session_name,
        reason.unwrap_or_default(),
    )) {
        warnings.push(format!("failed to write archive reason for '{name}': {e}"));
    }
    Ok(warnings)
}

/// Build the JSON result for a successful `archive`.
pub(crate) fn json_archive_result(
    name: &str,
    reason: Option<&str>,
    warnings: &[String],
    cfg: &Config,
) -> serde_json::Value {
    let agent = AgentRef::new(name, cfg);
    serde_json::json!({
        "ok": true,
        "agent": name,
        "branch": agent.branch_name(),
        "worktree": agent.worktree_path(cfg),
        "reason": reason,
        "warnings": warnings,
    })
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
    let agent = Agent::resolve(name, cfg)?;
    let content = ssh
        .run(&logs_snapshot_deep_command(name, TRANSCRIPT_MAX_LINES, cfg))
        .map_err(|e| agent.classify(e))?;
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

/// Send a prompt to a running agent and confirm the harness submitted it,
/// pressing Enter again while it has not (see [`confirm_submission`]).
///
/// A prompt left in the input box is reported, not an error: it reached the
/// agent, and the user can submit it by hand.
pub(crate) fn cmd_send(
    ssh: &impl Ssh,
    name: &str,
    prompt: &str,
    cfg: &Config,
    pacing: &SubmitPacing,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    let session_name = agent.session_name();
    ssh.run(&send_command(name, prompt, cfg))
        .map_err(|e| agent.classify(e))?;
    let submission = confirm_submission(ssh, &session_name, cfg, pacing);
    if cfg.output_format == OutputFormat::Json {
        emit_json(&json_send_result(name, submission));
        return Ok(());
    }
    match submission {
        Submission::Submitted => eprintln!("Prompt delivered to {session_name}."),
        Submission::NotSubmitted => eprintln!(
            "Warning: Prompt typed into {session_name} but not submitted; it is still in the \
             input box. Inspect with `skulk connect {name}` and press Enter."
        ),
    }
    Ok(())
}

/// Build the JSON result for `send`. `ok` is true once the prompt reached the
/// agent's input box; `prompt` says whether the harness submitted it.
fn json_send_result(name: &str, submission: Submission) -> serde_json::Value {
    let prompt = match submission {
        Submission::Submitted => "delivered",
        Submission::NotSubmitted => "not_submitted",
    };
    serde_json::json!({"ok": true, "agent": name, "prompt": prompt})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submit::probe_frame;
    use crate::testutil::PrivateTmux;
    use crate::testutil::{MockSsh, assert_err, ssh_err, ssh_ok, test_config, test_config_json};

    #[test]
    fn connect_command_generates_tmux_attach() {
        let cfg = test_config();
        let cmd = connect_command("my-task", &cfg);
        assert_eq!(
            cmd,
            "tmux set-option -t skulk-my-task status off; \
             skulk_win=$(tmux list-windows -t skulk-my-task -F '#{window_name}' 2>/dev/null | grep -qx agent && echo skulk-my-task:agent || echo 'skulk-my-task:^') && tmux attach-session -t \"$skulk_win\""
        );
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
        let ssh = MockSsh::new(vec![ssh_ok()]);
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
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("nope"));
        });
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
        let ssh = MockSsh::new(vec![ssh_ok()]);
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
    fn json_push_result_reports_agent_branch_and_remote() {
        let cfg = test_config_json();

        let value = json_push_result("my-task", &cfg, "Everything up-to-date\n");

        assert_eq!(value["ok"], true);
        assert_eq!(value["agent"], "my-task");
        assert_eq!(value["branch"], "skulk-my-task");
        assert_eq!(value["remote"], "origin");
        assert_eq!(value["output"], "Everything up-to-date");
    }

    #[test]
    fn cmd_push_in_json_mode_succeeds() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![Ok(String::new())]);

        assert!(cmd_push(&ssh, "my-task", &cfg).is_ok());
    }

    #[test]
    fn cmd_push_in_json_mode_returns_error_for_emit_json_error() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "error: src refspec skulk-my-task does not match any".into(),
        ))]);

        assert!(cmd_push(&ssh, "my-task", &cfg).is_err());
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
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("nope"));
        });
    }

    #[test]
    fn cmd_push_no_origin_remote() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "fatal: 'origin' does not appear to be a git repository\nfatal: Could not read from remote repository.".into(),
        ))]);
        let result = cmd_push(&ssh, "test", &cfg);
        assert_err!(result, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.to_lowercase().contains("origin"));
            assert_ne!(suggestion.len(), 0);
        });
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
        let ssh = MockSsh::new(vec![ssh_ok()]);
        assert!(cmd_disconnect(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_disconnect_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_disconnect(&ssh, "ghost", &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"));
        });
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
    fn logs_snapshot_deep_command_generates_capture_with_scrollback() {
        let cfg = test_config();
        let cmd = logs_snapshot_deep_command("my-task", 500, &cfg);
        assert_eq!(
            cmd,
            "skulk_win=$(tmux list-windows -t skulk-my-task -F '#{window_name}' 2>/dev/null | grep -qx agent && echo skulk-my-task:agent || echo 'skulk-my-task:^') && tmux capture-pane -p -t \"$skulk_win\" -S -500"
        );
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
        assert_eq!(
            cmd,
            "mkdir -p ~/.skulk/state && printf pending > ~/.skulk/state/skulk-my-task && \
             skulk_win=$(tmux list-windows -t skulk-my-task -F '#{window_name}' 2>/dev/null | grep -qx agent && echo skulk-my-task:agent || echo 'skulk-my-task:^') && \
             tmux send-keys -t \"$skulk_win\" 'fix the bug' && \
             sleep 0.200 && \
             tmux send-keys -t \"$skulk_win\" Enter"
        );
    }

    #[test]
    fn send_and_logs_reach_the_agent_window_while_server_window_is_active() {
        let Some(tmux) = PrivateTmux::start(0) else {
            return;
        };
        tmux.sh("tmux new-session -d -s skulk-my-task -n agent cat && \
             tmux new-window -t skulk-my-task -n server cat");
        let active = tmux.sh("tmux display-message -p -t skulk-my-task '#{window_name}'");
        assert_eq!(active.trim(), "server");
        let cfg = test_config();

        tmux.sh(&send_command("my-task", "MARK-SEND", &cfg));

        let logs = tmux.sh_until(&logs_command("my-task", None, None, &cfg), "MARK-SEND");
        assert!(logs.contains("MARK-SEND"), "logs: {logs}");
        let deep = tmux.sh(&logs_snapshot_deep_command("my-task", 50, &cfg));
        assert!(deep.contains("MARK-SEND"), "deep logs: {deep}");
        let agent = tmux.sh("tmux capture-pane -p -t skulk-my-task:agent");
        assert!(agent.contains("MARK-SEND"), "agent window: {agent}");
        let server = tmux.sh("tmux capture-pane -p -t skulk-my-task:server");
        assert!(!server.contains("MARK-SEND"), "server window: {server}");
    }

    // ── logs --window / restart-window ─────────────────────────────

    #[test]
    fn logs_command_with_window_captures_that_window() {
        let cfg = test_config();
        assert_eq!(
            logs_command("my-task", Some("server"), None, &cfg),
            "skulk_win=skulk-my-task:=server && tmux capture-pane -p -t \"$skulk_win\""
        );
    }

    #[test]
    fn logs_command_with_window_and_lines_captures_scrollback() {
        let cfg = test_config();
        assert_eq!(
            logs_command("my-task", Some("server"), Some(300), &cfg),
            "skulk_win=skulk-my-task:=server && tmux capture-pane -p -t \"$skulk_win\" -S -300"
        );
    }

    #[test]
    fn cmd_logs_with_window_captures_the_named_window() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("Listening on 3000".into())]);

        cmd_logs(&ssh, "test", false, Some(100), Some("server"), &cfg).expect("logs succeed");

        let calls = ssh.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].contains("skulk_win=skulk-test:=server") && calls[0].contains("-S -100"),
            "got: {}",
            calls[0]
        );
    }

    #[test]
    fn cmd_logs_returns_window_not_found_when_window_is_missing() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find window: server".into(),
        ))]);

        let result = cmd_logs(&ssh, "test", false, None, Some("server"), &cfg);

        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("Window 'server' not found in agent 'test'"), "{msg}");
        });
    }

    #[test]
    fn cmd_logs_rejects_invalid_window_name_without_ssh() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);

        let result = cmd_logs(&ssh, "test", false, None, Some("a;b"), &cfg);

        assert_err!(result, SkulkError::Validation(_) => {});
        assert_eq!(ssh.calls(), Vec::<String>::new());
    }

    #[test]
    fn cmd_restart_window_respawns_the_named_window() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        cmd_restart_window(&ssh, "test", "server", &cfg).expect("restart succeeds");

        let calls = ssh.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].ends_with("tmux respawn-window -k -t skulk-test:=server"),
            "got: {}",
            calls[0]
        );
    }

    #[test]
    fn cmd_restart_window_returns_window_not_found_when_window_is_missing() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find window: server".into(),
        ))]);

        let result = cmd_restart_window(&ssh, "test", "server", &cfg);

        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("Window 'server' not found"), "{msg}");
        });
    }

    #[test]
    fn cmd_restart_window_returns_agent_not_found_when_session_is_missing() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-gone".into(),
        ))]);

        let result = cmd_restart_window(&ssh, "gone", "server", &cfg);

        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("Agent 'gone' not found"), "{msg}");
        });
    }

    #[test]
    fn cmd_restart_window_refuses_the_agent_window() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);

        let result = cmd_restart_window(&ssh, "test", "agent", &cfg);

        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("skulk restart"), "{msg}");
        });
        assert_eq!(ssh.calls(), Vec::<String>::new());
    }

    #[test]
    fn cmd_restart_window_refuses_when_the_remote_finds_the_agent_window() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            remote::AGENT_WINDOW_REFUSAL.into(),
        ))]);

        let result = cmd_restart_window(&ssh, "test", "claude", &cfg);

        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("runs the agent itself"), "{msg}");
        });
    }

    #[test]
    fn cmd_restart_window_rejects_invalid_window_name_without_ssh() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);

        let result = cmd_restart_window(&ssh, "test", "$(id)", &cfg);

        assert_err!(result, SkulkError::Validation(_) => {});
        assert_eq!(ssh.calls(), Vec::<String>::new());
    }

    #[test]
    fn server_window_logs_and_restart_work_on_a_real_tmux() {
        let Some(tmux) = PrivateTmux::start(0) else {
            return;
        };
        let counter = tmux.dir().join("boots");
        let counter = counter.display();
        tmux.sh(&format!(
            "tmux new-session -d -s skulk-my-task -n agent cat && \
             tmux set-option -wg remain-on-exit on && \
             tmux new-window -d -t skulk-my-task -n server \
               'echo boot >> {counter}; echo BOOT-$(grep -c boot {counter}); exec cat'"
        ));
        let cfg = test_config();

        let first = tmux.sh_until(
            &logs_command("my-task", Some("server"), Some(50), &cfg),
            "BOOT-1",
        );
        assert!(first.contains("BOOT-1"), "server log: {first}");
        let agent = tmux.sh(&logs_command("my-task", None, None, &cfg));
        assert!(!agent.contains("BOOT-1"), "agent window: {agent}");

        tmux.sh(&remote_respawn_for_test("my-task", "server", &cfg));

        let second = tmux.sh_until(
            &logs_command("my-task", Some("server"), None, &cfg),
            "BOOT-2",
        );
        assert!(
            second.contains("BOOT-2"),
            "server log after restart: {second}"
        );
    }

    /// The shell text `cmd_restart_window` sends, captured through `MockSsh`
    /// so the real-tmux test runs exactly what production runs.
    fn remote_respawn_for_test(name: &str, window: &str, cfg: &Config) -> String {
        let ssh = MockSsh::new(vec![ssh_ok()]);
        cmd_restart_window(&ssh, name, window, cfg).expect("restart succeeds");
        ssh.calls().remove(0)
    }

    #[test]
    fn send_command_marks_pending_before_typing() {
        let cfg = test_config();
        let cmd = send_command("my-task", "fix the bug", &cfg);
        let busy_idx = cmd
            .find("printf pending")
            .expect("pending marker write missing");
        let type_idx = cmd
            .find("'fix the bug'")
            .expect("prompt typing step missing");
        assert!(
            busy_idx < type_idx,
            "pending marker must be written before send-keys: {cmd}"
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
        let ssh = MockSsh::new(vec![ssh_ok()]);
        assert!(cmd_connect(&ssh, "test", &cfg).is_ok());
    }

    #[test]
    fn cmd_connect_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_connect(&ssh, "ghost", &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"));
        });
    }

    #[test]
    fn cmd_logs_snapshot_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("line 1\nline 2\nline 3".into())]);
        assert!(cmd_logs(&ssh, "test", false, None, None, &cfg).is_ok());
    }

    #[test]
    fn cmd_logs_deep_snapshot_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("scrollback line 1\nscrollback line 2".into())]);
        assert!(cmd_logs(&ssh, "test", false, Some(500), None, &cfg).is_ok());
    }

    #[test]
    fn cmd_logs_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-nope".into(),
        ))]);
        let result = cmd_logs(&ssh, "nope", false, None, None, &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("nope"));
        });
    }

    #[test]
    fn cmd_logs_pane_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find pane: skulk-missing".into(),
        ))]);
        let result = cmd_logs(&ssh, "missing", false, None, None, &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("missing"));
        });
    }

    #[test]
    fn cmd_send_succeeds_when_hook_confirms_submission() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok(), probe_frame("busy", "")]);

        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg, &SubmitPacing::zero()).is_ok());
        assert_eq!(ssh.calls().len(), 2);
    }

    #[test]
    fn cmd_send_resends_enter_when_the_first_is_swallowed() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            ssh_ok(),
            probe_frame("pending", "> fix the bug"),
            ssh_ok(), // second Enter
            probe_frame("pending", "✻ Working… (esc to interrupt)"),
        ]);

        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg, &SubmitPacing::zero()).is_ok());
        assert!(ssh.calls()[2].ends_with("tmux send-keys -t \"$skulk_win\" Enter"));
    }

    #[test]
    fn cmd_send_still_succeeds_when_prompt_is_not_submitted() {
        let cfg = test_config();
        let stuck = || probe_frame("pending", "> fix the bug");
        let ssh = MockSsh::new(vec![
            ssh_ok(),
            stuck(),
            ssh_ok(),
            stuck(),
            ssh_ok(),
            stuck(),
        ]);

        assert!(cmd_send(&ssh, "test", "fix the bug", &cfg, &SubmitPacing::zero()).is_ok());
        assert_eq!(ssh.calls().len(), 6);
    }

    #[test]
    fn json_send_result_reports_delivered_or_not_submitted() {
        let delivered = json_send_result("my-task", Submission::Submitted);
        let stuck = json_send_result("my-task", Submission::NotSubmitted);

        assert_eq!(delivered["ok"], true);
        assert_eq!(delivered["agent"], "my-task");
        assert_eq!(delivered["prompt"], "delivered");
        assert_eq!(stuck["ok"], true);
        assert_eq!(stuck["prompt"], "not_submitted");
    }

    #[test]
    fn send_command_lets_a_large_prompt_settle_longer_before_enter() {
        let cfg = test_config();
        let cmd = send_command("my-task", &"x".repeat(10 * 1024), &cfg);
        assert!(
            cmd.contains("sleep 1.200 && "),
            "{}",
            &cmd[cmd.len() - 200..]
        );
    }

    #[test]
    fn cmd_send_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-gone".into(),
        ))]);
        let result = cmd_send(&ssh, "gone", "hello", &cfg, &SubmitPacing::zero());
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("gone"));
        });
    }

    #[test]
    fn cmd_send_pane_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find pane: skulk-missing".into(),
        ))]);
        let result = cmd_send(&ssh, "missing", "hello", &cfg, &SubmitPacing::zero());
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("missing"));
        });
    }

    #[test]
    fn cmd_archive_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok(), ssh_ok()]);
        assert!(cmd_archive(&ssh, "test", None, &cfg).is_ok());
    }

    #[test]
    fn json_archive_result_reports_preserved_worktree_and_reason() {
        let cfg = test_config_json();

        let value = json_archive_result("my-task", Some("PR merged"), &[], &cfg);

        assert_eq!(value["ok"], true);
        assert_eq!(value["agent"], "my-task");
        assert_eq!(value["branch"], "skulk-my-task");
        assert_eq!(value["worktree"], "~/test-project-worktrees/skulk-my-task");
        assert_eq!(value["reason"], "PR merged");
        assert_eq!(value["warnings"], serde_json::json!([]));
    }

    #[test]
    fn json_archive_result_without_reason_has_null_reason() {
        let cfg = test_config_json();

        let value = json_archive_result("my-task", None, &[], &cfg);

        assert!(value["reason"].is_null());
    }

    #[test]
    fn archive_warnings_record_a_failed_reason_write() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![ssh_ok(), ssh_err("disk full")]);

        let warnings = archive_agent(&ssh, "my-task", Some("PR merged"), &cfg).unwrap();

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("archive reason"), "{}", warnings[0]);
    }

    #[test]
    fn cmd_archive_in_json_mode_succeeds() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![ssh_ok(), ssh_ok()]);

        assert!(cmd_archive(&ssh, "test", None, &cfg).is_ok());
    }

    #[test]
    fn cmd_archive_in_json_mode_returns_error_for_emit_json_error() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);

        let result = cmd_archive(&ssh, "ghost", None, &cfg);

        assert_err!(result, SkulkError::NotFound(_) => {});
    }

    #[test]
    fn cmd_archive_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_archive(&ssh, "ghost", None, &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"));
        });
    }

    #[test]
    fn cmd_archive_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_archive(&ssh, "../bad", None, &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn archive_with_reason_writes_sidecar() {
        // Two SSH calls: kill session, then write reason sidecar
        let ssh = MockSsh::new(vec![ssh_ok(), ssh_ok()]);
        let cfg = test_config();
        cmd_archive(&ssh, "my-agent", Some("PR merged"), &cfg).unwrap();
        let calls = ssh.calls();
        assert_eq!(calls.len(), 2);
        // Second call writes the sidecar
        assert!(calls[1].contains("skulk-my-agent.txt"));
        assert!(calls[1].contains("PR merged"));
        assert!(calls[1].contains("~/.skulk/archive"));
    }

    #[test]
    fn archive_without_reason_writes_empty_sidecar() {
        let ssh = MockSsh::new(vec![ssh_ok(), ssh_ok()]);
        let cfg = test_config();

        cmd_archive(&ssh, "my-agent", None, &cfg).unwrap();

        let calls = ssh.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], archive_reason_command("skulk-my-agent", ""));
    }

    #[test]
    fn archive_reason_failure_is_nonfatal() {
        // First call (kill) succeeds; second call (reason write) fails
        let ssh = MockSsh::new(vec![ssh_ok(), ssh_err("disk full")]);
        let cfg = test_config();
        // Must return Ok even though the sidecar write failed
        let result = cmd_archive(&ssh, "my-agent", Some("done"), &cfg);
        assert!(result.is_ok());
    }

    #[test]
    fn archive_reason_command_contains_session_name() {
        let cmd = archive_reason_command("skulk-my-agent", "PR merged");
        assert!(cmd.contains("skulk-my-agent.txt"));
    }

    #[test]
    fn archive_reason_command_contains_reason_text() {
        let cmd = archive_reason_command("skulk-my-agent", "PR merged");
        assert!(cmd.contains("PR merged"));
    }

    #[test]
    fn archive_reason_command_escapes_single_quotes() {
        // Reason text with single quotes must be shell-escaped
        let cmd = archive_reason_command("skulk-foo", "it's done");
        // shell_escape turns ' into '\''
        assert!(cmd.contains("it'\\''s done"));
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
    fn git_log_json_command_limits_and_uses_nul_separated_format() {
        let cfg = test_config();
        let cmd = git_log_json_command("feat", 5, &cfg);
        assert!(
            cmd.starts_with("cd \"$HOME\"/'test-project' && git log 'main..skulk-feat' -n 5 -z"),
            "{cmd}"
        );
        assert!(cmd.contains("--format='%H%x1f%h%x1f%aI%x1f%cI%x1f%an%x1f%s'"));
    }

    #[test]
    fn git_log_json_command_quotes_base_path_and_branch() {
        let mut cfg = test_config();
        cfg.base_path = "/srv/my proj's".into();
        cfg.default_branch = "rel/1;x".into();
        let cmd = git_log_json_command("feat", 1, &cfg);
        assert!(cmd.starts_with("cd '/srv/my proj'\\''s' && git log 'rel/1;x..skulk-feat'"));
    }

    #[test]
    fn json_git_log_result_parses_records_in_order() {
        let cfg = test_config();
        let out = "aaa\u{1f}a\u{1f}2026-01-01T00:00:00+00:00\u{1f}2026-01-02T00:00:00+00:00\u{1f}Ann\u{1f}say \"hi\" é\0\
                   bbb\u{1f}b\u{1f}2025-01-01T00:00:00+00:00\u{1f}2025-01-01T00:00:00+00:00\u{1f}Bob\u{1f}first\0";
        let v = json_git_log_result("feat", &cfg, out).expect("parses");
        assert_eq!(v["v"], 1);
        assert_eq!(v["agent"], "feat");
        assert_eq!(v["base"], "main");
        assert_eq!(v["commits"][0]["subject"], "say \"hi\" é");
        assert_eq!(v["commits"][0]["author"], "Ann");
        assert_eq!(v["commits"][0]["authored_at"], "2026-01-01T00:00:00+00:00");
        assert_eq!(v["commits"][0]["committed_at"], "2026-01-02T00:00:00+00:00");
        assert_eq!(v["commits"][1]["short"], "b");
        assert_eq!(v["commits"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn json_git_log_result_keeps_subject_containing_the_separator() {
        let cfg = test_config();
        let out = "aaa\u{1f}a\u{1f}t1\u{1f}t2\u{1f}Ann\u{1f}one\u{1f}two\u{1f}\u{1f}three\0";
        let v = json_git_log_result("feat", &cfg, out).expect("parses");
        assert_eq!(v["commits"][0]["subject"], "one\u{1f}two\u{1f}\u{1f}three");
        assert_eq!(v["commits"][0]["author"], "Ann");
    }

    #[test]
    fn json_git_log_result_accepts_an_empty_subject() {
        let cfg = test_config();
        let out = "aaa\u{1f}a\u{1f}t1\u{1f}t2\u{1f}Ann\u{1f}\0";
        let v = json_git_log_result("feat", &cfg, out).expect("parses");
        assert_eq!(v["commits"][0]["subject"], "");
        assert_eq!(v["commits"][0]["author"], "Ann");
    }

    #[test]
    fn json_git_log_result_is_empty_for_no_output() {
        let cfg = test_config();
        let v = json_git_log_result("feat", &cfg, "").expect("parses");
        assert_eq!(v["commits"], serde_json::json!([]));
    }

    #[test]
    fn json_git_log_result_rejects_record_with_too_few_fields() {
        let cfg = test_config();
        assert!(json_git_log_result("feat", &cfg, "only\u{1f}two\0").is_err());
    }

    #[test]
    fn cmd_git_log_succeeds_and_prints_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("abc1234 first commit\ndef5678 second".into())]);
        assert!(cmd_git_log(&ssh, "test", 200, &cfg).is_ok());
    }

    #[test]
    fn cmd_git_log_returns_empty_output_when_no_commits() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);
        assert!(cmd_git_log(&ssh, "test", 200, &cfg).is_ok());
    }

    #[test]
    fn cmd_git_log_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "fatal: ambiguous argument 'main..skulk-nope': unknown revision or path not in the working tree".into(),
        ))]);
        let result = cmd_git_log(&ssh, "nope", 200, &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("nope"));
        });
    }

    #[test]
    fn cmd_git_log_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_git_log(&ssh, "../bad", 200, &cfg);
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
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("Failed to write"), "unexpected message: {msg}");
        });
    }

    #[test]
    fn cmd_transcript_agent_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "can't find session: skulk-ghost".into(),
        ))]);
        let result = cmd_transcript(&ssh, "ghost", None, &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"));
        });
    }

    #[test]
    fn cmd_transcript_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_transcript(&ssh, "../bad", None, &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    // ── helpers for JSON result shape tests ────────────────────────────────

    fn json_logs_result(name: &str, raw: &str) -> serde_json::Value {
        let lines: Vec<&str> = raw.lines().collect();
        serde_json::json!({
            "agent": name,
            "lines": lines,
        })
    }

    fn json_diff_result(name: &str, diff: &str) -> serde_json::Value {
        serde_json::json!({
            "agent": name,
            "diff": diff,
        })
    }

    // ── logs JSON mode ──────────────────────────────────────────────────────

    #[test]
    fn logs_json_mode_emits_agent_and_lines_array() {
        let raw = "line one\nline two\nline three";
        let result = json_logs_result("my-agent", raw);
        assert_eq!(result["agent"], "my-agent");
        let lines = result["lines"].as_array().expect("lines should be array");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line one");
        assert_eq!(lines[1], "line two");
        assert_eq!(lines[2], "line three");
    }

    #[test]
    fn logs_human_mode_returns_raw_output() {
        let mut cfg = test_config();
        cfg.output_format = OutputFormat::Human;
        let ssh = MockSsh::new(vec![Ok("line 1\nline 2\nline 3".into())]);
        assert!(cmd_logs(&ssh, "test", false, None, None, &cfg).is_ok());
    }

    // ── diff JSON mode ──────────────────────────────────────────────────────

    #[test]
    fn diff_json_mode_emits_agent_and_diff_string() {
        let raw = "diff --git a/foo b/foo\n+added line\n-removed line";
        let result = json_diff_result("my-agent", raw);
        assert_eq!(result["agent"], "my-agent");
        assert_eq!(result["diff"], raw);
    }

    #[test]
    fn diff_human_mode_returns_raw_output() {
        let mut cfg = test_config();
        cfg.output_format = OutputFormat::Human;
        let ssh = MockSsh::new(vec![Ok("diff --git a/foo b/foo\n+hello".into())]);
        assert!(cmd_diff(&ssh, "test", DiffFormat::Default, &cfg).is_ok());
    }

    #[test]
    fn diff_format_from_flags_maps_each_flag() {
        assert_eq!(DiffFormat::from_flags(false, false), DiffFormat::Default);
        assert_eq!(DiffFormat::from_flags(true, false), DiffFormat::Stat);
        assert_eq!(DiffFormat::from_flags(false, true), DiffFormat::NameOnly);
    }

    #[test]
    fn resolve_send_prompt_uses_positional_text() {
        let result = resolve_send_prompt(Some("fix it".into()), None);
        assert_eq!(result.ok().as_deref(), Some("fix it"));
    }

    #[test]
    fn resolve_send_prompt_reads_file_and_trims_trailing_newline() {
        let path = std::env::temp_dir().join("skulk_resolve_send_prompt_test.txt");
        std::fs::write(&path, "line one\nline two\n").expect("write temp prompt file");
        let result = resolve_send_prompt(None, Some(&path));
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.ok().as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn resolve_send_prompt_reports_missing_file_as_validation_error() {
        let result = resolve_send_prompt(None, Some(Path::new("/nonexistent/skulk_absent.txt")));
        assert_err!(result, SkulkError::Validation(_) => {});
    }
}
