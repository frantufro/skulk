use std::collections::HashMap;

use crate::config::Config;
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
    let worktrees = get_worktree_map(&worktrees_raw, cfg);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_config;

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
