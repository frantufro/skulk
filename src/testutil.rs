use std::cell::RefCell;
use std::collections::VecDeque;

use crate::commands::init::Prompter;
use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;

/// Builds a `Config` with known values for testing.
pub(crate) fn test_config() -> Config {
    Config {
        host: "testhost".to_string(),
        session_prefix: "skulk-".to_string(),
        base_path: "~/test-project".to_string(),
        worktree_base: "~/test-project-worktrees".to_string(),
        default_branch: "main".to_string(),
    }
}

pub(crate) struct MockSsh {
    pub responses: RefCell<VecDeque<Result<String, SkulkError>>>,
    calls: RefCell<Vec<String>>,
}

impl MockSsh {
    pub fn new(responses: Vec<Result<String, SkulkError>>) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Returns the commands passed to `run` and `interactive`, in call order.
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
pub(crate) fn mock_list_output(epoch: i64, tmux_lines: &str, worktrees: &[(&str, &str)]) -> String {
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
    out
}
