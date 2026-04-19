use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

use crate::commands::init::Prompter;
use crate::config::Config;
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
pub(crate) fn ssh_ok() -> Result<String, SkulkError> {
    Ok(String::new())
}

/// Short-hand for an `SshFailed` error with the given message.
pub(crate) fn ssh_err(msg: &str) -> Result<String, SkulkError> {
    Err(SkulkError::SshFailed(msg.to_string()))
}

/// Builds a `Config` with known values for testing.
pub(crate) fn test_config() -> Config {
    Config {
        host: "testhost".to_string(),
        session_prefix: "skulk-".to_string(),
        base_path: "~/test-project".to_string(),
        worktree_base: "~/test-project-worktrees".to_string(),
        default_branch: "main".to_string(),
        init_script: None,
        root_dir: None,
    }
}

pub(crate) struct MockSsh {
    pub responses: RefCell<VecDeque<Result<String, SkulkError>>>,
    pub upload_responses: RefCell<VecDeque<Result<(), SkulkError>>>,
    calls: RefCell<Vec<String>>,
}

impl MockSsh {
    pub fn new(responses: Vec<Result<String, SkulkError>>) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
            upload_responses: RefCell::new(VecDeque::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Queue responses for `upload_file` calls. If the queue is empty when
    /// `upload_file` is called, the mock returns `Ok(())`.
    pub fn with_upload_responses(mut self, responses: Vec<Result<(), SkulkError>>) -> Self {
        self.upload_responses = RefCell::new(responses.into());
        self
    }

    /// Returns the commands passed to `run`, `interactive`, and `upload_file`,
    /// in call order. `upload_file` calls are recorded as
    /// `UPLOAD <local>:<remote>` strings so tests can assert ordering.
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
        out.push_str(&format!(
            "worktree {path}\nHEAD abc123\nbranch refs/heads/{branch}\n\n"
        ));
    }
    out.push_str("__WORKTREES_END__\n");
    out.push_str("__BRANCHES_START__\n");
    for b in branches {
        out.push_str(&format!("  {b}\n"));
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

/// Helper: build a mock list_command response with epoch, tmux sessions, and worktrees.
/// The state section is emitted empty; use [`mock_list_output_with_state`] to
/// populate Stop-hook state files for idle-column tests.
pub(crate) fn mock_list_output(epoch: i64, tmux_lines: &str, worktrees: &[(&str, &str)]) -> String {
    mock_list_output_with_state(epoch, tmux_lines, worktrees, &[])
}

/// Helper: build a mock `status_command` response.
///
/// Sections mirror the delimited layout emitted by `status_command`: epoch,
/// tmux, worktrees porcelain, branch-exists probe, Stop-hook state mtime,
/// rev-list count, and `git diff --stat` summary line.
///
/// `tmux_lines` is inserted verbatim so callers can also pass a `no server
/// running…` line to exercise the stopped-agent path.
pub(crate) fn mock_status_output(
    epoch: i64,
    tmux_lines: &str,
    worktrees: &[(&str, &str)],
    branch_exists: bool,
    state_mtime: Option<i64>,
    commits_ahead: Option<u32>,
    diffstat_line: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("__EPOCH__{epoch}__EPOCH__\n"));
    out.push_str("__TMUX_START__\n");
    out.push_str(tmux_lines);
    if !tmux_lines.is_empty() && !tmux_lines.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("__TMUX_END__\n");
    out.push_str("__WORKTREES_START__\n");
    for (branch, path) in worktrees {
        out.push_str(&format!(
            "worktree {path}\nHEAD abc123\nbranch refs/heads/{branch}\n\n"
        ));
    }
    out.push_str("__WORKTREES_END__\n");
    out.push_str("__BRANCH_EXISTS_START__\n");
    out.push_str(if branch_exists { "yes\n" } else { "no\n" });
    out.push_str("__BRANCH_EXISTS_END__\n");
    out.push_str("__STATE_START__\n");
    if let Some(m) = state_mtime {
        out.push_str(&format!("{m}\n"));
    }
    out.push_str("__STATE_END__\n");
    out.push_str("__REVCOUNT_START__\n");
    if let Some(c) = commits_ahead {
        out.push_str(&format!("{c}\n"));
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

/// Helper: like [`mock_list_output`] but also injects Stop-hook state entries
/// as `(session_name, mtime_epoch)` pairs.
pub(crate) fn mock_list_output_with_state(
    epoch: i64,
    tmux_lines: &str,
    worktrees: &[(&str, &str)],
    state: &[(&str, i64)],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("__EPOCH__{epoch}__EPOCH__\n"));
    out.push_str("__TMUX_START__\n");
    out.push_str(tmux_lines);
    if !tmux_lines.is_empty() && !tmux_lines.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("__TMUX_END__\n");
    out.push_str("__WORKTREES_START__\n");
    for (branch, path) in worktrees {
        out.push_str(&format!(
            "worktree {path}\nHEAD abc123\nbranch refs/heads/{branch}\n\n"
        ));
    }
    out.push_str("__WORKTREES_END__\n");
    out.push_str("__STATE_START__\n");
    for (name, mtime) in state {
        out.push_str(&format!("{name} {mtime}\n"));
    }
    out.push_str("__STATE_END__\n");
    out
}
