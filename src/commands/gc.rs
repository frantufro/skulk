use std::collections::HashSet;

use crate::agent_ref::AgentRef;
use crate::commands::destroy::{
    agent_destroy_branch_command, agent_destroy_session_command, agent_destroy_worktree_command,
};
use crate::config::Config;
use crate::display::format_gc_summary;
use crate::error::SkulkError;
use crate::inventory::{AgentInventory, GcOrphans, fetch_inventory};
use crate::ssh::Ssh;

/// Analyze an `AgentInventory` and find orphaned resources.
///
/// Orphan definitions:
/// - Orphaned session: tmux session with session prefix but no matching worktree or branch
/// - Orphaned worktree: worktree with session prefix but no matching session AND no matching branch
///   (a worktree with its branch intact is an *archived* agent -- preserved for `skulk restart`)
/// - Orphaned branch: branch with session prefix but no matching session or worktree
pub(crate) fn gc_find_orphans(inv: &AgentInventory) -> GcOrphans {
    let session_set: HashSet<&str> = inv.sessions.iter().map(String::as_str).collect();
    let worktree_set: HashSet<&str> = inv.worktrees.keys().map(String::as_str).collect();
    let branch_set: HashSet<&str> = inv.branches.iter().map(String::as_str).collect();

    // Orphaned sessions: have session but no worktree AND no branch
    let mut sessions: Vec<String> = session_set
        .iter()
        .filter(|s| !worktree_set.contains(*s) && !branch_set.contains(*s))
        .map(ToString::to_string)
        .collect();

    // Orphaned worktrees: have worktree but no session AND no branch.
    // A worktree whose branch still exists is an *archived* agent (killed tmux
    // session, work preserved for `skulk restart`) and must not be reaped here.
    let mut worktrees: Vec<String> = worktree_set
        .iter()
        .filter(|w| !session_set.contains(*w) && !branch_set.contains(*w))
        .map(ToString::to_string)
        .collect();

    // Orphaned branches: have branch but no session AND no worktree
    let mut branches: Vec<String> = branch_set
        .iter()
        .filter(|b| !session_set.contains(*b) && !worktree_set.contains(*b))
        .map(ToString::to_string)
        .collect();

    sessions.sort();
    worktrees.sort();
    branches.sort();

    GcOrphans {
        sessions,
        worktrees,
        branches,
    }
}

pub(crate) fn cmd_gc(ssh: &impl Ssh, dry_run: bool, cfg: &Config) -> Result<(), SkulkError> {
    let base_path = &cfg.base_path;

    // Fetch comprehensive inventory
    let inv = fetch_inventory(ssh, cfg)?;

    // Find orphans
    let orphans = gc_find_orphans(&inv);

    if orphans.is_empty() {
        println!("{}", format_gc_summary(&orphans, dry_run));
        return Ok(());
    }

    if dry_run {
        println!("{}", format_gc_summary(&orphans, true));
        return Ok(());
    }

    // Clean up orphaned sessions
    for session in &orphans.sessions {
        let agent = AgentRef::from_qualified(session, cfg);
        eprint!("  Killing orphaned session {session}... ");
        if ssh
            .run(&agent_destroy_session_command(agent.name(), cfg))
            .is_ok()
        {
            eprintln!("done");
        } else {
            eprintln!("failed");
        }
    }

    // Clean up orphaned worktrees (and their branches)
    for worktree in &orphans.worktrees {
        let agent = AgentRef::from_qualified(worktree, cfg);
        eprint!("  Removing orphaned worktree {worktree}... ");
        let wt_ok = ssh
            .run(&agent_destroy_worktree_command(agent.name(), cfg))
            .is_ok();
        let br_ok = ssh
            .run(&agent_destroy_branch_command(agent.name(), cfg))
            .is_ok();
        if wt_ok && br_ok {
            eprintln!("done");
        } else {
            eprintln!("done (with warnings)");
        }
    }

    // Clean up orphaned branches (no session or worktree)
    for branch in &orphans.branches {
        let agent = AgentRef::from_qualified(branch, cfg);
        eprint!("  Deleting orphaned branch {branch}... ");
        if ssh
            .run(&agent_destroy_branch_command(agent.name(), cfg))
            .is_ok()
        {
            eprintln!("done");
        } else {
            eprintln!("failed");
        }
    }

    // Also prune worktree references
    let _ = ssh.run(&format!("cd {base_path} && git worktree prune"));

    println!("\n{}", format_gc_summary(&orphans, false));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, make_inventory, mock_inventory, mock_inventory_single_agent, ssh_ok, test_config,
    };

    #[test]
    fn gc_find_orphans_no_orphans() {
        let inv = make_inventory(
            &["skulk-task1"],
            &[("skulk-task1", "/path/skulk-task1")],
            &["skulk-task1"],
        );
        let orphans = gc_find_orphans(&inv);
        assert!(orphans.is_empty());
    }

    #[test]
    fn gc_find_orphans_orphaned_session() {
        let inv = make_inventory(&["skulk-ghost"], &[], &[]);
        let orphans = gc_find_orphans(&inv);
        assert_eq!(orphans.sessions, vec!["skulk-ghost"]);
        assert!(orphans.worktrees.is_empty());
        assert!(orphans.branches.is_empty());
    }

    #[test]
    fn gc_find_orphans_orphaned_worktree() {
        // Truly dangling: worktree directory tracked by git but with no matching
        // branch (e.g. after a manual `git branch -D`). Still safe to reap.
        let inv = make_inventory(&[], &[("skulk-stale", "/path/skulk-stale")], &[]);
        let orphans = gc_find_orphans(&inv);
        assert!(orphans.sessions.is_empty());
        assert_eq!(orphans.worktrees, vec!["skulk-stale"]);
        assert!(orphans.branches.is_empty());
    }

    #[test]
    fn gc_find_orphans_archived_agent_not_reaped() {
        // Archived state: worktree + branch present, session killed. Must be
        // preserved so `skulk restart` can resume the agent.
        let inv = make_inventory(
            &[],
            &[("skulk-archived", "/path/skulk-archived")],
            &["skulk-archived"],
        );
        let orphans = gc_find_orphans(&inv);
        assert!(orphans.sessions.is_empty());
        assert!(
            orphans.worktrees.is_empty(),
            "archived worktree must not be reaped"
        );
        assert!(
            orphans.branches.is_empty(),
            "archived branch must not be reaped"
        );
    }

    #[test]
    fn gc_find_orphans_orphaned_branch() {
        let inv = make_inventory(&[], &[], &["skulk-leftover"]);
        let orphans = gc_find_orphans(&inv);
        assert!(orphans.sessions.is_empty());
        assert!(orphans.worktrees.is_empty());
        assert_eq!(orphans.branches, vec!["skulk-leftover"]);
    }

    #[test]
    fn gc_find_orphans_mixed() {
        // `skulk-stale-wt` has no branch listed -- truly dangling, not archived.
        let inv = make_inventory(
            &["skulk-healthy", "skulk-ghost-sess"],
            &[
                ("skulk-healthy", "/path/skulk-healthy"),
                ("skulk-stale-wt", "/path/skulk-stale-wt"),
            ],
            &["skulk-healthy"],
        );
        let orphans = gc_find_orphans(&inv);
        assert_eq!(orphans.sessions, vec!["skulk-ghost-sess"]);
        assert_eq!(orphans.worktrees, vec!["skulk-stale-wt"]);
        assert!(orphans.branches.is_empty());
    }

    #[test]
    fn gc_find_orphans_empty_inventory() {
        let inv = make_inventory(&[], &[], &[]);
        let orphans = gc_find_orphans(&inv);
        assert!(orphans.is_empty());
        assert_eq!(orphans.total(), 0);
    }

    #[test]
    fn gc_find_orphans_all_orphaned() {
        let inv = make_inventory(
            &["skulk-sess-only"],
            &[("skulk-wt-only", "/path/skulk-wt-only")],
            &["skulk-br-only"],
        );
        let orphans = gc_find_orphans(&inv);
        assert_eq!(orphans.sessions.len(), 1);
        assert_eq!(orphans.worktrees.len(), 1);
        assert_eq!(orphans.branches.len(), 1);
        assert_eq!(orphans.total(), 3);
    }

    #[test]
    fn gc_find_orphans_multiple_healthy_agents() {
        let inv = make_inventory(
            &["skulk-a", "skulk-b", "skulk-c"],
            &[
                ("skulk-a", "/path/skulk-a"),
                ("skulk-b", "/path/skulk-b"),
                ("skulk-c", "/path/skulk-c"),
            ],
            &["skulk-a", "skulk-b", "skulk-c"],
        );
        let orphans = gc_find_orphans(&inv);
        assert!(orphans.is_empty());
    }

    #[test]
    fn gc_session_with_branch_not_orphaned() {
        let inv = make_inventory(&["skulk-running"], &[], &["skulk-running"]);
        let orphans = gc_find_orphans(&inv);
        assert!(orphans.sessions.is_empty());
        assert!(orphans.branches.is_empty());
    }

    #[test]
    fn cmd_gc_dry_run_with_orphans() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(&["skulk-ghost"], &[], &[]))]);
        assert!(cmd_gc(&ssh, true, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_clean_state() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory_single_agent("skulk-healthy"))]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_cleans_orphaned_session() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-ghost"], &[], &[])),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_cleans_orphaned_worktree() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            // Branch absent -- truly dangling worktree, safe to reap.
            Ok(mock_inventory(
                &[],
                &[("skulk-stale", "/path/skulk-stale")],
                &[],
            )),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_archived_agent_preserved() {
        // End-to-end: gc must not touch an archived agent's worktree or branch.
        // Only the inventory SSH call plus the bookkeeping `git worktree prune`
        // should run; no destroy calls.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(
            &[],
            &[("skulk-archived", "/path/skulk-archived")],
            &["skulk-archived"],
        ))]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        assert_eq!(
            ssh.calls().len(),
            1,
            "only the inventory call should have run, got: {:?}",
            ssh.calls()
        );
    }

    #[test]
    fn cmd_gc_cleans_orphaned_branch() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-leftover"])),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_dry_run_does_not_modify() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(
            &["skulk-ghost"],
            &[("skulk-stale", "/path/skulk-stale")],
            &["skulk-leftover"],
        ))]);
        assert!(cmd_gc(&ssh, true, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_session_cleanup_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-ghost"], &[], &[])),
            Err(SkulkError::SshFailed("kill-session failed".into())),
            ssh_ok(),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_worktree_cleanup_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-stale", "/path/skulk-stale")],
                &[],
            )),
            Err(SkulkError::SshFailed("worktree remove failed".into())),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_branch_cleanup_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-leftover"])),
            Err(SkulkError::SshFailed("branch delete failed".into())),
            ssh_ok(),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }
}
