use std::collections::HashSet;

use crate::agent_ref::AgentRef;
use crate::commands::destroy::{
    agent_destroy_branch_command, agent_destroy_session_command, agent_destroy_state_file_command,
    agent_destroy_worktree_command,
};
use crate::config::Config;
use crate::display::format_gc_summary;
use crate::error::SkulkError;
use crate::inventory::{AgentInventory, GcOrphans, fetch_inventory};
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command to list Stop-hook state files on the remote.
///
/// Emits one filename per line to stdout. A missing `~/.skulk/state/`
/// directory is not an error — the `|| true` fallback makes the probe
/// return an empty result, which the caller treats as "nothing to clean".
pub(crate) fn list_state_files_command() -> String {
    "ls ~/.skulk/state/ 2>/dev/null || true".to_string()
}

/// Remove `~/.skulk/state/<session>` markers whose owning tmux session no
/// longer exists. Runs after orphan session/worktree/branch cleanup so that
/// sessions just killed by gc are already considered "gone" here.
///
/// A filename is an orphan when:
///   1. It starts with the configured `session_prefix`,
///   2. the suffix is a valid agent name (defense against weird filenames
///      landing unsanitized in a `rm -f` command), AND
///   3. its session is not in the *surviving* set
///      (`inv.sessions` minus `orphans.sessions`).
///
/// Cleanup is silent unless stale entries are found, so routine gc runs don't
/// emit noise when the state dir is clean.
fn cleanup_stale_state_files(
    ssh: &impl Ssh,
    inv: &AgentInventory,
    orphans: &GcOrphans,
    cfg: &Config,
) {
    let state_raw = ssh.run(&list_state_files_command()).unwrap_or_default();

    let orphan_sess: HashSet<&str> = orphans.sessions.iter().map(String::as_str).collect();
    let surviving: HashSet<&str> = inv
        .sessions
        .iter()
        .map(String::as_str)
        .filter(|s| !orphan_sess.contains(s))
        .collect();

    let stale: Vec<String> = state_raw
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.starts_with(&*cfg.session_prefix))
        .filter(|s| {
            let suffix = &s[cfg.session_prefix.len()..];
            validate_name(suffix).is_ok()
        })
        .filter(|s| !surviving.contains(s))
        .map(ToString::to_string)
        .collect();

    for name in &stale {
        let agent = AgentRef::from_qualified(name, cfg);
        eprint!("  Removing stale state file {name}... ");
        if ssh
            .run(&agent_destroy_state_file_command(agent.name(), cfg))
            .is_ok()
        {
            eprintln!("done");
        } else {
            eprintln!("failed");
        }
    }
}

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

    // Also prune worktree references (only meaningful after touching worktrees,
    // but cheap enough to run whenever we had any orphans).
    if !orphans.is_empty() {
        let _ = ssh.run(&format!("cd {base_path} && git worktree prune"));
    }

    // Reap stale `~/.skulk/state/<session>` markers left behind by destroy/gc
    // paths that predate state-file cleanup, or by tmux servers that died
    // without the destroy path running. Runs even when no other orphans were
    // found, since state files can leak independently of session/worktree state.
    cleanup_stale_state_files(ssh, &inv, &orphans, cfg);

    let prefix = if orphans.is_empty() { "" } else { "\n" };
    println!("{prefix}{}", format_gc_summary(&orphans, false));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, make_inventory, mock_empty_inventory, mock_inventory, mock_inventory_single_agent,
        ssh_ok, test_config,
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
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-healthy")),
            // State dir empty -- nothing to reap.
            Ok(String::new()),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_cleans_orphaned_session() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-ghost"], &[], &[])),
            ssh_ok(),          // kill session
            ssh_ok(),          // worktree prune
            Ok(String::new()), // state ls -- empty
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
            ssh_ok(),          // worktree remove
            ssh_ok(),          // branch delete
            ssh_ok(),          // worktree prune
            Ok(String::new()), // state ls -- empty
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_archived_agent_preserved() {
        // End-to-end: gc must not touch an archived agent's worktree or branch.
        // Only the inventory probe plus the state-file listing should run; no
        // destroy calls. (State listing is empty here -- see the companion
        // `cmd_gc_archived_agent_state_file_reaped` test for the case where a
        // state file does exist.)
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-archived", "/path/skulk-archived")],
                &["skulk-archived"],
            )),
            Ok(String::new()), // state ls -- empty
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        let calls = ssh.calls();
        assert_eq!(
            calls.len(),
            2,
            "expected only inventory + state ls, got: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("git worktree remove")
                || c.contains("git branch -D")
                || c.contains("tmux kill-session")),
            "no destroy calls should run for an archived agent: {calls:?}"
        );
    }

    #[test]
    fn cmd_gc_cleans_orphaned_branch() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-leftover"])),
            ssh_ok(),          // branch delete
            ssh_ok(),          // worktree prune
            Ok(String::new()), // state ls -- empty
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
            ssh_ok(),          // worktree prune
            Ok(String::new()), // state ls -- empty
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
            ssh_ok(),          // branch delete
            ssh_ok(),          // worktree prune
            Ok(String::new()), // state ls -- empty
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_gc_branch_cleanup_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-leftover"])),
            Err(SkulkError::SshFailed("branch delete failed".into())),
            ssh_ok(),          // worktree prune
            Ok(String::new()), // state ls -- empty
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
    }

    // ── State-file cleanup ──────────────────────────────────────────────

    #[test]
    fn list_state_files_command_uses_rm_f_semantics_for_missing_dir() {
        // `|| true` guards against the dir not existing so gc doesn't error
        // on a fresh host before any agent has been created.
        let cmd = list_state_files_command();
        assert!(cmd.contains("ls ~/.skulk/state/"), "got: {cmd}");
        assert!(cmd.contains("|| true"), "got: {cmd}");
    }

    #[test]
    fn cmd_gc_removes_stale_state_file_with_no_session() {
        // State file exists for a session that isn't running and has no
        // worktree/branch -- classic leak (hook fired, then destroy cleaned
        // up everything but the marker on a pre-cleanup version of skulk).
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            Ok("skulk-stale\n".to_string()), // state ls
            ssh_ok(),                        // rm -f ~/.skulk/state/skulk-stale
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "rm -f ~/.skulk/state/skulk-stale"),
            "expected rm of stale state file, got: {calls:?}"
        );
    }

    #[test]
    fn cmd_gc_preserves_state_file_for_live_session() {
        // Live healthy agent -- its state file must be preserved.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-live")),
            Ok("skulk-live\n".to_string()), // state ls lists the live session
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        let calls = ssh.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("rm -f ~/.skulk/state/")),
            "must not rm state file for live session: {calls:?}"
        );
    }

    #[test]
    fn cmd_gc_reaps_state_for_killed_orphan_session() {
        // Orphan session gets killed during gc; its state file should also be
        // reaped (the session is no longer in the "surviving" set).
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-ghost"], &[], &[])),
            ssh_ok(),                        // kill session
            ssh_ok(),                        // worktree prune
            Ok("skulk-ghost\n".to_string()), // state ls
            ssh_ok(),                        // rm -f ~/.skulk/state/skulk-ghost
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "rm -f ~/.skulk/state/skulk-ghost"),
            "expected rm of state file for killed orphan: {calls:?}"
        );
    }

    #[test]
    fn cmd_gc_archived_agent_state_file_reaped() {
        // Archived agent (no session, has worktree+branch): worktree/branch
        // are preserved for `skulk restart`, but the state file is tied to
        // the tmux session lifetime and is reaped. A fresh marker is written
        // on the next turn after restart.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-archived", "/path/skulk-archived")],
                &["skulk-archived"],
            )),
            Ok("skulk-archived\n".to_string()), // state ls
            ssh_ok(),                           // rm -f ~/.skulk/state/skulk-archived
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "rm -f ~/.skulk/state/skulk-archived"),
            "archived agent's state file should be reaped: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("git worktree remove") || c.contains("git branch -D")),
            "archived worktree + branch must not be touched: {calls:?}"
        );
    }

    #[test]
    fn cmd_gc_ignores_non_prefixed_state_entries() {
        // Another tool could share `~/.skulk/state/`. Don't touch its files.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            Ok("other-tool-data\nunrelated\n".to_string()),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        let calls = ssh.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("rm -f ~/.skulk/state/")),
            "must not rm entries outside our prefix: {calls:?}"
        );
    }

    #[test]
    fn cmd_gc_ignores_state_entries_with_invalid_names() {
        // Defense-in-depth: if something bizarre landed in the state dir
        // (a file named `skulk-with spaces` or `skulk-rogue;rm`), we don't
        // pass it to `rm`. `validate_name` rejects anything outside
        // `[a-z0-9-]` so the suffix can't smuggle in shell metacharacters.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_empty_inventory()),
            Ok("skulk-UPPER\nskulk-with space\nskulk-semi;colon\n".to_string()),
        ]);
        assert!(cmd_gc(&ssh, false, &cfg).is_ok());
        let calls = ssh.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("rm -f ~/.skulk/state/")),
            "must not rm invalid-named entries: {calls:?}"
        );
    }

    #[test]
    fn cmd_gc_dry_run_skips_state_cleanup() {
        // Dry run must be side-effect free: no state ls, no rm. The caller
        // wants to see what *would* be touched, and (for now) state files
        // aren't reported in the summary; skipping the probe entirely keeps
        // dry-run fully observational.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        assert!(cmd_gc(&ssh, true, &cfg).is_ok());
        assert_eq!(
            ssh.calls().len(),
            1,
            "dry-run should only probe inventory, got: {:?}",
            ssh.calls()
        );
    }
}
