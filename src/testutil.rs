use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use crate::commands::init::Prompter;
use crate::config::{Config, OutputFormat};
use crate::error::SkulkError;
use crate::inventory::AgentInventory;
use crate::ssh::Ssh;

/// Matches a `Result<_, SkulkError>` against an expected error variant.
///
/// The `Ok` arm panics with a clear message; a mismatched error variant panics
/// with the actual variant so test failures are diagnosable.
///
/// ```ignore
/// assert_err!(result, SkulkError::NotFound(msg) => {
///     assert!(msg.contains("foo"));
/// });
/// ```
macro_rules! assert_err {
    ($result:expr, $variant:pat => $body:block) => {
        match $result.expect_err("expected Err, got Ok") {
            $variant => $body,
            other => panic!("unexpected error variant: {other:?}"),
        }
    };
}
pub(crate) use assert_err;

/// Short-hand for a successful, empty SSH response.
///
/// Returns a wrapped `Ok` rather than the bare `String` so the same call shape
/// can sit next to [`ssh_err`] inside `MockSsh::new(vec![...])`, which expects
/// `Result<String, SkulkError>`. Removing the wrap would force every call site
/// to write `Ok(ssh_ok())` instead.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn ssh_ok() -> Result<String, SkulkError> {
    Ok(String::new())
}

/// Short-hand for an `SshFailed` error with the given message.
pub(crate) fn ssh_err(msg: &str) -> Result<String, SkulkError> {
    Err(SkulkError::SshFailed(msg.to_string()))
}

/// Builds a `Config` with known values for testing.
pub(crate) fn test_config() -> Config {
    test_config_with_format(OutputFormat::Human)
}

/// Builds a `Config` with known values for testing, with the given output format.
pub(crate) fn test_config_with_format(output_format: OutputFormat) -> Config {
    Config {
        host: "testhost".to_string(),
        session_prefix: "skulk-".to_string(),
        base_path: "~/test-project".to_string(),
        worktree_base: "~/test-project-worktrees".to_string(),
        default_branch: "main".to_string(),
        harness: "claude".to_string(),
        init_script: None,
        auto_approve_permissions: false,
        output_format,
        root_dir: None,
    }
}

/// Builds a `Config` set to JSON output mode for testing JSON-output paths.
pub(crate) fn test_config_json() -> Config {
    Config {
        output_format: OutputFormat::Json,
        ..test_config()
    }
}

pub(crate) struct MockSsh {
    pub responses: RefCell<VecDeque<Result<String, SkulkError>>>,
    pub upload_responses: RefCell<VecDeque<Result<(), SkulkError>>>,
    pub download_responses: RefCell<VecDeque<Result<(), SkulkError>>>,
    calls: RefCell<Vec<String>>,
}

impl MockSsh {
    pub fn new(responses: Vec<Result<String, SkulkError>>) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
            upload_responses: RefCell::new(VecDeque::new()),
            download_responses: RefCell::new(VecDeque::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Queue responses for `upload_file` calls. If the queue is empty when
    /// `upload_file` is called, the mock returns `Ok(())`.
    pub fn with_upload_responses(mut self, responses: Vec<Result<(), SkulkError>>) -> Self {
        self.upload_responses = RefCell::new(responses.into());
        self
    }

    /// Queue responses for `download_file` calls. If the queue is empty when
    /// `download_file` is called, the mock returns `Ok(())`.
    pub fn with_download_responses(mut self, responses: Vec<Result<(), SkulkError>>) -> Self {
        self.download_responses = RefCell::new(responses.into());
        self
    }

    /// Returns the commands passed to `run`, `interactive`, `upload_file`, and
    /// `download_file`, in call order. `upload_file` calls are recorded as
    /// `UPLOAD <local>:<remote>` and `download_file` calls as
    /// `DOWNLOAD <remote>:<local>` strings so tests can assert ordering.
    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Ssh for MockSsh {
    fn run(&self, cmd: &str) -> Result<String, SkulkError> {
        self.calls.borrow_mut().push(cmd.to_string());
        self.responses
            .borrow_mut()
            .pop_front()
            .expect("MockSsh: unexpected extra SSH call")
    }

    fn interactive(&self, cmd: &str) -> Result<std::process::ExitStatus, SkulkError> {
        self.calls.borrow_mut().push(cmd.to_string());
        Ok(std::process::ExitStatus::default())
    }

    fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<(), SkulkError> {
        self.calls
            .borrow_mut()
            .push(format!("UPLOAD {}:{remote_path}", local_path.display()));
        self.upload_responses
            .borrow_mut()
            .pop_front()
            .unwrap_or(Ok(()))
    }

    fn download_file(&self, remote_path: &str, local_path: &Path) -> Result<(), SkulkError> {
        self.calls
            .borrow_mut()
            .push(format!("DOWNLOAD {remote_path}:{}", local_path.display()));
        self.download_responses
            .borrow_mut()
            .pop_front()
            .unwrap_or(Ok(()))
    }
}

/// Helper: shorthand for `mock_inventory(&[], &[], &[])`.
pub(crate) fn mock_empty_inventory() -> String {
    mock_inventory(&[], &[], &[])
}

/// Helper: fully-healthy single-agent inventory. `name` is the full
/// session-prefixed name (e.g. `"skulk-target"`); session, worktree branch,
/// and worktree path all key off that name.
pub(crate) fn mock_inventory_single_agent(name: &str) -> String {
    let path = format!("/path/{name}");
    mock_inventory(&[name], &[(name, &path)], &[name])
}

/// Helper: build an `AgentInventory` struct directly. Used by tests that
/// exercise pure-logic functions consuming the struct (e.g. `gc_find_orphans`).
/// `worktrees` is given as `(branch, path)` pairs for parity with
/// [`mock_inventory`].
pub(crate) fn make_inventory(
    sessions: &[&str],
    worktrees: &[(&str, &str)],
    branches: &[&str],
) -> AgentInventory {
    let mut worktree_map = HashMap::new();
    for (branch, path) in worktrees {
        worktree_map.insert((*branch).to_string(), (*path).to_string());
    }
    AgentInventory {
        sessions: sessions.iter().map(|s| (*s).to_string()).collect(),
        worktrees: worktree_map,
        branches: branches.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Helper: build a mock inventory response with the given sessions, worktrees, and branches.
pub(crate) fn mock_inventory(
    sessions: &[&str],
    worktrees: &[(&str, &str)],
    branches: &[&str],
) -> String {
    let mut out = String::new();
    out.push_str("__SESSIONS_START__\n");
    for s in sessions {
        out.push_str(s);
        out.push('\n');
    }
    out.push_str("__SESSIONS_END__\n");
    out.push_str("__WORKTREES_START__\n");
    for (branch, path) in worktrees {
        writeln!(
            out,
            "worktree {path}\nHEAD abc123\nbranch refs/heads/{branch}\n"
        )
        .expect("writing to String is infallible");
    }
    out.push_str("__WORKTREES_END__\n");
    out.push_str("__BRANCHES_START__\n");
    for b in branches {
        writeln!(out, "  {b}").expect("writing to String is infallible");
    }
    out.push_str("__BRANCHES_END__\n");
    out
}

pub(crate) struct MockPrompter {
    responses: VecDeque<String>,
}

impl MockPrompter {
    pub fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: responses.into_iter().map(ToString::to_string).collect(),
        }
    }
}

impl Prompter for MockPrompter {
    fn prompt(&mut self, _message: &str) -> Result<String, SkulkError> {
        self.responses
            .pop_front()
            .ok_or_else(|| SkulkError::Validation("MockPrompter: no more responses".into()))
    }

    fn confirm(&mut self, _message: &str, default_yes: bool) -> Result<bool, SkulkError> {
        let response = self
            .responses
            .pop_front()
            .ok_or_else(|| SkulkError::Validation("MockPrompter: no more responses".into()))?;
        let answer = response.trim().to_lowercase();
        if answer.is_empty() {
            return Ok(default_yes);
        }
        Ok(answer == "y" || answer == "yes")
    }
}

/// Helper: build a mock `list_command` response with epoch, tmux sessions, and worktrees.
/// The state section is emitted empty; use [`mock_list_output_with_state`] to
/// populate Stop-hook state files for idle-column tests.
pub(crate) fn mock_list_output(epoch: i64, tmux_lines: &str, worktrees: &[(&str, &str)]) -> String {
    mock_list_output_with_state(epoch, tmux_lines, worktrees, &[])
}

/// Helper: build a mock `status_command` response.
///
/// Sections mirror the delimited layout emitted by `status_command`: epoch,
/// tmux, worktrees porcelain, branch-exists probe, Stop/`UserPromptSubmit`
/// hook marker contents, rev-list count, and `git diff --stat` summary line.
///
/// `tmux_lines` is inserted verbatim so callers can also pass a `no server
/// running…` line to exercise the stopped-agent path.
pub(crate) fn mock_status_output(
    epoch: i64,
    tmux_lines: &str,
    worktrees: &[(&str, &str)],
    branch_exists: bool,
    state_marker: Option<&str>,
    commits_ahead: Option<u32>,
    diffstat_line: &str,
) -> String {
    let mut out = String::new();
    writeln!(out, "__EPOCH__{epoch}__EPOCH__").expect("writing to String is infallible");
    out.push_str("__TMUX_START__\n");
    out.push_str(tmux_lines);
    if !tmux_lines.is_empty() && !tmux_lines.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("__TMUX_END__\n");
    out.push_str("__WORKTREES_START__\n");
    for (branch, path) in worktrees {
        writeln!(
            out,
            "worktree {path}\nHEAD abc123\nbranch refs/heads/{branch}\n"
        )
        .expect("writing to String is infallible");
    }
    out.push_str("__WORKTREES_END__\n");
    out.push_str("__BRANCH_EXISTS_START__\n");
    out.push_str(if branch_exists { "yes\n" } else { "no\n" });
    out.push_str("__BRANCH_EXISTS_END__\n");
    out.push_str("__STATE_START__\n");
    if let Some(m) = state_marker {
        out.push_str(m);
        if !m.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("__STATE_END__\n");
    out.push_str("__REVCOUNT_START__\n");
    if let Some(c) = commits_ahead {
        writeln!(out, "{c}").expect("writing to String is infallible");
    }
    out.push_str("__REVCOUNT_END__\n");
    out.push_str("__DIFFSTAT_START__\n");
    out.push_str(diffstat_line);
    if !diffstat_line.is_empty() && !diffstat_line.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("__DIFFSTAT_END__\n");
    out
}

/// Helper: like [`mock_list_output`] but also injects Stop/`UserPromptSubmit`
/// hook marker entries as `(session_name, marker_content)` pairs, where
/// `marker_content` is the literal string the hooks would have written
/// (`"idle"` or `"busy"`).
pub(crate) fn mock_list_output_with_state(
    epoch: i64,
    tmux_lines: &str,
    worktrees: &[(&str, &str)],
    state: &[(&str, &str)],
) -> String {
    let mut out = String::new();
    writeln!(out, "__EPOCH__{epoch}__EPOCH__").expect("writing to String is infallible");
    out.push_str("__TMUX_START__\n");
    out.push_str(tmux_lines);
    if !tmux_lines.is_empty() && !tmux_lines.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("__TMUX_END__\n");
    out.push_str("__WORKTREES_START__\n");
    for (branch, path) in worktrees {
        writeln!(
            out,
            "worktree {path}\nHEAD abc123\nbranch refs/heads/{branch}\n"
        )
        .expect("writing to String is infallible");
    }
    out.push_str("__WORKTREES_END__\n");
    out.push_str("__STATE_START__\n");
    for (name, content) in state {
        writeln!(out, "{name} {content}").expect("writing to String is infallible");
    }
    out.push_str("__STATE_END__\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_download_file_records_call() {
        let ssh = MockSsh::new(vec![]);
        ssh.download_file("~/.claude/projects/x/a.jsonl", Path::new("/local/a.jsonl"))
            .expect("default download should be Ok");
        let calls = ssh.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            "DOWNLOAD ~/.claude/projects/x/a.jsonl:/local/a.jsonl"
        );
    }

    #[test]
    fn mock_download_file_default_ok() {
        // No responses queued — the mock returns Ok by default.
        let ssh = MockSsh::new(vec![]);
        let result = ssh.download_file("remote", Path::new("/local"));
        assert!(result.is_ok());
    }

    #[test]
    fn mock_download_file_honors_queued_responses() {
        let ssh = MockSsh::new(vec![])
            .with_download_responses(vec![Err(SkulkError::SshFailed("scp failed".into()))]);
        let result = ssh.download_file("remote", Path::new("/local"));
        assert!(matches!(result, Err(SkulkError::SshFailed(_))));
    }
}
