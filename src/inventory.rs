use std::collections::HashMap;

use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::extract_section;

/// Remote agent state gathered in a single SSH round-trip.
#[derive(Debug, Clone)]
pub(crate) struct AgentInventory {
    /// tmux session names with session prefix
    pub sessions: Vec<String>,
    /// branch name -> worktree path (prefix-matching only)
    pub worktrees: HashMap<String, String>,
    /// branch names with session prefix
    pub branches: Vec<String>,
}

/// Lifecycle state of a Skulk agent.
///
/// `Attached` / `Detached` refer to the tmux session's `session_attached`
/// flag: `Attached` means at least one client is connected. `Idle` supersedes
/// both when the Stop-hook marker is newer than the session's last activity,
/// signalling that the agent has finished its turn and is awaiting input.
/// `Stopped` means the tmux session is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentState {
    Attached,
    Detached,
    Idle,
    Stopped,
}

#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub name: String,
    pub created: i64,
    /// tmux `session_activity` epoch — last time any pane output changed.
    pub activity: i64,
    pub state: AgentState,
    pub worktree: Option<String>,
}

/// Parse a raw tmux `list-sessions` output into `Session`s with an initial
/// `Attached` or `Detached` state. Callers upgrade to `Idle` via
/// [`resolve_agent_state`] once the Stop-hook mtime is known.
pub(crate) fn parse_sessions(raw: &str) -> Vec<Session> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 4 {
                return None;
            }
            let created = parts[1].parse::<i64>().ok()?;
            let activity = parts[2].parse::<i64>().ok()?;
            let state = if parts[3] == "0" {
                AgentState::Detached
            } else {
                AgentState::Attached
            };
            Some(Session {
                name: parts[0].to_string(),
                created,
                activity,
                state,
                worktree: None,
            })
        })
        .collect()
}

/// Upgrade a live session's state to `Idle` when the Stop-hook marker is at
/// least as recent as the tmux activity timestamp. `Stopped` is preserved.
///
/// The Stop hook fires just after Claude writes its last output, so when idle
/// `state_mtime >= activity`. When Claude resumes, new output bumps `activity`
/// above `state_mtime` until the next turn ends.
pub(crate) fn resolve_agent_state(
    state: &AgentState,
    activity: i64,
    state_mtime: Option<i64>,
) -> AgentState {
    if *state == AgentState::Stopped {
        return AgentState::Stopped;
    }
    match state_mtime {
        Some(m) if m >= activity => AgentState::Idle,
        _ => state.clone(),
    }
}

/// Orphaned resources identified by gc.
#[derive(Debug, Clone)]
pub(crate) struct GcOrphans {
    /// tmux sessions with session prefix but no matching worktree
    pub sessions: Vec<String>,
    /// worktrees with session prefix but no matching tmux session
    pub worktrees: Vec<String>,
    /// branches with session prefix but no matching session or worktree
    pub branches: Vec<String>,
}

impl GcOrphans {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.worktrees.is_empty() && self.branches.is_empty()
    }

    pub fn total(&self) -> usize {
        self.sessions.len() + self.worktrees.len() + self.branches.len()
    }
}

/// Parse `git worktree list --porcelain` output into a map of `branch_name` -> `worktree_path`.
/// Only includes branches matching the configured session prefix.
///
/// Porcelain format has blocks separated by blank lines. Each block starts with
/// `worktree /path` and may contain `branch refs/heads/name` plus other metadata
/// lines (HEAD, detached, prunable) which are ignored.
pub(crate) fn get_worktree_map(porcelain_output: &str, cfg: &Config) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut current_path: Option<String> = None;

    for line in porcelain_output.lines() {
        if line.is_empty() {
            // Block separator -- reset
            current_path = None;
        } else if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if let Some(branch_ref) = line.strip_prefix("branch refs/heads/")
            && branch_ref.starts_with(&*cfg.session_prefix)
            && let Some(ref path) = current_path
        {
            map.insert(branch_ref.to_string(), path.clone());
        }
        // Ignore other lines (HEAD, detached, prunable, etc.)
    }

    map
}

/// Build the SSH command that gathers all agent state in one round-trip.
/// Returns a shell command string that outputs delimited sections for
/// tmux sessions, git worktrees, and git branches.
pub(crate) fn inventory_command(cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let session_prefix = &cfg.session_prefix;
    format!(
        "echo __SESSIONS_START__ && \
         tmux list-sessions -F '#{{session_name}}' 2>/dev/null || true && \
         echo __SESSIONS_END__ && \
         echo __WORKTREES_START__ && \
         cd {base_path} && git worktree list --porcelain 2>/dev/null || true && \
         echo __WORKTREES_END__ && \
         echo __BRANCHES_START__ && \
         cd {base_path} && git branch --list '{session_prefix}*' 2>/dev/null || true && \
         echo __BRANCHES_END__"
    )
}

/// Parse the raw output of the inventory command into structured `AgentInventory`.
///
/// Expects delimited sections:
/// - `__SESSIONS_START__` ... `__SESSIONS_END__` — tmux session names (one per line)
/// - `__WORKTREES_START__` ... `__WORKTREES_END__` — git worktree list --porcelain output
/// - `__BRANCHES_START__` ... `__BRANCHES_END__` — git branch --list output (leading spaces trimmed)
pub(crate) fn parse_inventory(raw: &str, cfg: &Config) -> AgentInventory {
    // Sessions: lines between markers, filtered to session prefix
    let sessions_raw = extract_section(raw, "__SESSIONS_START__\n", "\n__SESSIONS_END__");
    let sessions: Vec<String> = sessions_raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.starts_with(&*cfg.session_prefix))
        .map(ToString::to_string)
        .collect();

    // Worktrees: parse porcelain output between markers
    let worktrees_raw = extract_section(raw, "__WORKTREES_START__\n", "\n__WORKTREES_END__");
    let worktrees = get_worktree_map(worktrees_raw, cfg);

    // Branches: lines between markers, trimmed, filtered to session prefix
    let branches_raw = extract_section(raw, "__BRANCHES_START__\n", "\n__BRANCHES_END__");
    let branches: Vec<String> = branches_raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.starts_with(&*cfg.session_prefix))
        .map(ToString::to_string)
        .collect();

    AgentInventory {
        sessions,
        worktrees,
        branches,
    }
}

/// Round-trip helper: run the inventory probe over SSH and parse the result.
///
/// Most callers don't need the raw bytes — they just want an `AgentInventory`.
/// Wraps the single-round-trip SSH call + parse so the same two-line idiom
/// doesn't appear in every command module.
pub(crate) fn fetch_inventory(ssh: &impl Ssh, cfg: &Config) -> Result<AgentInventory, SkulkError> {
    let raw = ssh.run(&inventory_command(cfg))?;
    Ok(parse_inventory(&raw, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_config;

    // ── parse_sessions ──────────────────────────────────────────────────

    #[test]
    fn parse_sessions_single_line() {
        let raw = "skulk-test\t1700000000\t1700000100\t0\n";
        let sessions = parse_sessions(raw);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "skulk-test");
        assert_eq!(sessions[0].created, 1700000000);
        assert_eq!(sessions[0].activity, 1700000100);
        assert_eq!(sessions[0].state, AgentState::Detached);
        assert!(sessions[0].worktree.is_none());
    }

    #[test]
    fn parse_sessions_empty_input() {
        let sessions = parse_sessions("");
        assert!(sessions.is_empty());
    }

    #[test]
    fn parse_sessions_malformed_skipped() {
        let sessions = parse_sessions("bad\tdata");
        assert!(sessions.is_empty());
    }

    #[test]
    fn parse_sessions_multiple_lines() {
        let raw = "skulk-a\t1\t2\t0\nskulk-b\t3\t4\t1\n";
        let sessions = parse_sessions(raw);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].state, AgentState::Detached);
        assert_eq!(sessions[1].state, AgentState::Attached);
    }

    // ── resolve_agent_state ─────────────────────────────────────────────

    #[test]
    fn resolve_agent_state_stopped_when_session_gone() {
        assert_eq!(
            resolve_agent_state(&AgentState::Stopped, 0, None),
            AgentState::Stopped
        );
        assert_eq!(
            resolve_agent_state(&AgentState::Stopped, 1000, Some(2000)),
            AgentState::Stopped
        );
    }

    #[test]
    fn resolve_agent_state_preserves_live_state_without_marker() {
        assert_eq!(
            resolve_agent_state(&AgentState::Detached, 1000, None),
            AgentState::Detached
        );
        assert_eq!(
            resolve_agent_state(&AgentState::Attached, 1000, None),
            AgentState::Attached
        );
    }

    #[test]
    fn resolve_agent_state_idle_when_mtime_ge_activity() {
        assert_eq!(
            resolve_agent_state(&AgentState::Detached, 1000, Some(1000)),
            AgentState::Idle
        );
        assert_eq!(
            resolve_agent_state(&AgentState::Attached, 1000, Some(1005)),
            AgentState::Idle
        );
    }

    #[test]
    fn resolve_agent_state_working_when_activity_after_mtime() {
        assert_eq!(
            resolve_agent_state(&AgentState::Detached, 2000, Some(1000)),
            AgentState::Detached
        );
        assert_eq!(
            resolve_agent_state(&AgentState::Attached, 2000, Some(1000)),
            AgentState::Attached
        );
    }

    // ── GcOrphans ───────────────────────────────────────────────────────

    #[test]
    fn gc_orphans_total() {
        let orphans = GcOrphans {
            sessions: vec!["a".into()],
            worktrees: vec!["b".into(), "c".into()],
            branches: vec!["d".into()],
        };
        assert_eq!(orphans.total(), 4);
        assert!(!orphans.is_empty());
    }

    // ── worktree map / inventory ────────────────────────────────────────

    #[test]
    fn worktree_map_parses_agent_entries() {
        let cfg = test_config();
        let input = "\
worktree /home/user/claude-consultancy
HEAD abc123
branch refs/heads/main

worktree /home/user/claude-consultancy-agents/skulk-my-task
HEAD def456
branch refs/heads/skulk-my-task

";
        let map = get_worktree_map(input, &cfg);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get("skulk-my-task").unwrap(),
            "/home/user/claude-consultancy-agents/skulk-my-task"
        );
    }

    #[test]
    fn worktree_map_skips_non_agent() {
        let cfg = test_config();
        let input = "\
worktree /home/user/project
HEAD abc123
branch refs/heads/main

worktree /home/user/project-feature
HEAD def456
branch refs/heads/feature-x

";
        let map = get_worktree_map(input, &cfg);
        assert!(map.is_empty());
    }

    #[test]
    fn worktree_map_empty_input() {
        let cfg = test_config();
        let map = get_worktree_map("", &cfg);
        assert!(map.is_empty());
    }

    #[test]
    fn worktree_map_with_extra_metadata() {
        let cfg = test_config();
        let input = "\
worktree /home/user/claude-consultancy-agents/skulk-test
HEAD abc123
branch refs/heads/skulk-test
prunable

worktree /home/user/other
HEAD def456
detached

";
        let map = get_worktree_map(input, &cfg);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get("skulk-test").unwrap(),
            "/home/user/claude-consultancy-agents/skulk-test"
        );
    }

    #[test]
    fn parse_inventory_full() {
        let cfg = test_config();
        let raw = "\
__SESSIONS_START__
skulk-task1
skulk-task2
other-session
__SESSIONS_END__
__WORKTREES_START__
worktree /home/user/claude-consultancy
HEAD abc123
branch refs/heads/main

worktree /home/user/claude-consultancy-agents/skulk-task1
HEAD def456
branch refs/heads/skulk-task1

__WORKTREES_END__
__BRANCHES_START__
  skulk-task1
  skulk-task2
  feature-unrelated
__BRANCHES_END__
";
        let inv = parse_inventory(raw, &cfg);
        assert_eq!(inv.sessions, vec!["skulk-task1", "skulk-task2"]);
        assert_eq!(inv.worktrees.len(), 1);
        assert_eq!(
            inv.worktrees.get("skulk-task1").unwrap(),
            "/home/user/claude-consultancy-agents/skulk-task1"
        );
        assert_eq!(inv.branches, vec!["skulk-task1", "skulk-task2"]);
    }

    #[test]
    fn parse_inventory_empty_sections() {
        let cfg = test_config();
        let raw = "\
__SESSIONS_START__
__SESSIONS_END__
__WORKTREES_START__
__WORKTREES_END__
__BRANCHES_START__
__BRANCHES_END__
";
        let inv = parse_inventory(raw, &cfg);
        assert!(inv.sessions.is_empty());
        assert!(inv.worktrees.is_empty());
        assert!(inv.branches.is_empty());
    }

    #[test]
    fn parse_inventory_partial_state() {
        let cfg = test_config();
        let raw = "\
__SESSIONS_START__
skulk-orphan
__SESSIONS_END__
__WORKTREES_START__
worktree /home/user/claude-consultancy
HEAD abc123
branch refs/heads/main

__WORKTREES_END__
__BRANCHES_START__
  skulk-orphan
__BRANCHES_END__
";
        let inv = parse_inventory(raw, &cfg);
        assert_eq!(inv.sessions, vec!["skulk-orphan"]);
        assert!(inv.worktrees.is_empty());
        assert_eq!(inv.branches, vec!["skulk-orphan"]);
    }
}
