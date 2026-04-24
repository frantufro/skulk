use std::collections::HashSet;

use crate::agent_ref::AgentRef;
use crate::config::Config;
use crate::error::SkulkError;
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command to kill a tmux session for an agent.
pub(crate) fn agent_destroy_session_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("tmux kill-session -t {}", agent.session_name())
}

/// Build the SSH command to remove an agent's git worktree.
pub(crate) fn agent_destroy_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let agent = AgentRef::new(name, cfg);
    format!(
        "cd {base_path} && git worktree remove --force {}",
        agent.worktree_path(cfg)
    )
}

/// Build the SSH command to delete an agent's git branch.
pub(crate) fn agent_destroy_branch_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let agent = AgentRef::new(name, cfg);
    format!("cd {base_path} && git branch -D {}", agent.branch_name())
}

/// Build the SSH command to delete an agent's Stop-hook state file.
///
/// The file lives at `~/.skulk/state/<session_name>` and is written by the
/// Claude Code `Stop` / `UserPromptSubmit` hooks (see `hooks_settings_json`).
/// `rm -f` makes a missing file a no-op so callers fire this unconditionally.
pub(crate) fn agent_destroy_state_file_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("rm -f ~/.skulk/state/{}", agent.session_name())
}

/// Run the branch-delete SSH command and record the outcome.
///
/// Used by both the `has_worktree` arm (worktree + branch cleanup) and the
/// orphaned-branch arm (branch only), which share identical push logic.
fn try_delete_branch<'a>(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
    cleaned: &mut Vec<&'a str>,
    failed: &mut Vec<&'a str>,
) {
    if ssh.run(&agent_destroy_branch_command(name, cfg)).is_ok() {
        cleaned.push("branch");
    } else {
        failed.push("branch");
    }
}

/// Destroy a specific agent (kills session, removes worktree, deletes branch).
///
/// Uses shared inventory to probe what exists.
/// Each cleanup step is independent -- if tmux is gone but worktree remains,
/// the worktree is still removed. Only errors if nothing exists at all.
pub(crate) fn cmd_destroy(
    ssh: &impl Ssh,
    name: &str,
    force: bool,
    cfg: &Config,
    confirm: &dyn Fn(&str) -> bool,
) -> Result<(), SkulkError> {
    validate_name(name)?;

    // Fetch inventory via shared probe
    let inv = fetch_inventory(ssh, cfg)?;
    let session_name = AgentRef::new(name, cfg).session_name();
    let has_session = inv.sessions.contains(&session_name);
    let has_worktree = inv.worktrees.contains_key(&session_name);
    let has_branch = inv.branches.contains(&session_name);

    if !has_session && !has_worktree && !has_branch {
        return Err(SkulkError::NotFound(format!("No agent '{name}' found.")));
    }

    if !force && !confirm(&format!("Destroy agent '{name}'? [y/N]")) {
        println!("Aborted.");
        return Ok(());
    }

    let mut cleaned: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();

    if has_session {
        if ssh.run(&agent_destroy_session_command(name, cfg)).is_ok() {
            cleaned.push("tmux session");
        } else {
            failed.push("tmux session");
        }
    }

    if has_worktree {
        if ssh.run(&agent_destroy_worktree_command(name, cfg)).is_ok() {
            cleaned.push("worktree");
        } else {
            failed.push("worktree");
        }
        try_delete_branch(ssh, name, cfg, &mut cleaned, &mut failed);
    } else if has_branch {
        // Orphaned branch (no worktree)
        try_delete_branch(ssh, name, cfg, &mut cleaned, &mut failed);
    }

    // Always reap the Stop-hook state marker. It's invisible plumbing for
    // `skulk wait`, so we don't add it to `cleaned` / `failed`; `rm -f` makes
    // a missing file a no-op anyway.
    let _ = ssh.run(&agent_destroy_state_file_command(name, cfg));

    if !cleaned.is_empty() {
        println!("Destroyed agent '{name}' ({}).", cleaned.join(", "));
    }
    if !failed.is_empty() {
        eprintln!(
            "Warning: failed to clean up {} for agent '{name}'. Run `skulk gc` to retry.",
            failed.join(", ")
        );
    }
    Ok(())
}

/// Destroy all agents (sessions, worktrees, branches).
///
/// Uses comprehensive inventory to discover ALL agents including orphans.
/// Builds unique agent name set from sessions + worktrees + branches.
pub(crate) fn cmd_destroy_all(
    ssh: &impl Ssh,
    force: bool,
    cfg: &Config,
    confirm: &dyn Fn(&str) -> bool,
) -> Result<(), SkulkError> {
    let inv = fetch_inventory(ssh, cfg)?;

    // Build comprehensive agent name set. Inventory entries are prefix-filtered
    // upstream, so `AgentRef::from_qualified` always recovers a bare agent name.
    let mut name_set: HashSet<String> = HashSet::new();
    for s in &inv.sessions {
        name_set.insert(AgentRef::from_qualified(s, cfg).name().to_string());
    }
    for key in inv.worktrees.keys() {
        name_set.insert(AgentRef::from_qualified(key, cfg).name().to_string());
    }
    for b in &inv.branches {
        name_set.insert(AgentRef::from_qualified(b, cfg).name().to_string());
    }
    let mut agent_names: Vec<String> = name_set.into_iter().collect();
    agent_names.sort();

    if agent_names.is_empty() {
        println!("No agents to destroy.");
        return Ok(());
    }

    if !force && !confirm(&format!("Destroy {} agent(s)? [y/N]", agent_names.len())) {
        println!("Aborted.");
        return Ok(());
    }

    let mut warned_count: usize = 0;
    for name in &agent_names {
        let agent = AgentRef::new(name, cfg);
        let session_name = agent.session_name();
        print!("Destroying {session_name}... ");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        let mut step_failed = false;
        if inv.sessions.contains(&session_name)
            && ssh.run(&agent_destroy_session_command(name, cfg)).is_err()
        {
            step_failed = true;
        }
        if inv.worktrees.contains_key(&session_name)
            && ssh.run(&agent_destroy_worktree_command(name, cfg)).is_err()
        {
            step_failed = true;
        }
        if inv.branches.contains(&session_name)
            && ssh.run(&agent_destroy_branch_command(name, cfg)).is_err()
        {
            step_failed = true;
        }

        // Reap the Stop-hook state marker for this agent. Fire-and-forget;
        // `rm -f` makes missing files a no-op and state cleanup is invisible
        // plumbing, not something worth surfacing in the summary.
        let _ = ssh.run(&agent_destroy_state_file_command(name, cfg));

        if step_failed {
            println!("done (with warnings)");
            warned_count += 1;
        } else {
            println!("done");
        }
    }

    let clean_count = agent_names.len() - warned_count;
    if warned_count == 0 {
        println!("Destroyed {} agent(s).", agent_names.len());
    } else {
        println!("Destroyed {clean_count} agent(s), {warned_count} with warnings.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, mock_empty_inventory, mock_inventory, mock_inventory_single_agent,
        ssh_err, ssh_ok, test_config,
    };

    #[test]
    fn agent_destroy_session_command_generates_kill() {
        let cfg = test_config();
        let cmd = agent_destroy_session_command("my-task", &cfg);
        assert_eq!(cmd, "tmux kill-session -t skulk-my-task");
    }

    #[test]
    fn agent_destroy_worktree_command_generates_remove() {
        let cfg = test_config();
        let cmd = agent_destroy_worktree_command("my-task", &cfg);
        assert!(cmd.contains("git worktree remove --force ~/test-project-worktrees/skulk-my-task"));
        assert!(cmd.starts_with("cd ~/test-project"));
    }

    #[test]
    fn agent_destroy_branch_command_generates_delete() {
        let cfg = test_config();
        let cmd = agent_destroy_branch_command("my-task", &cfg);
        assert!(cmd.contains("git branch -D skulk-my-task"));
        assert!(cmd.starts_with("cd ~/test-project"));
    }

    #[test]
    fn agent_destroy_state_file_command_generates_rm_f() {
        let cfg = test_config();
        let cmd = agent_destroy_state_file_command("my-task", &cfg);
        assert_eq!(cmd, "rm -f ~/.skulk/state/skulk-my-task");
    }

    fn confirm_yes(_: &str) -> bool {
        true
    }

    fn confirm_no(_: &str) -> bool {
        false
    }

    #[test]
    fn cmd_destroy_force_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(), // kill session
            ssh_ok(), // remove worktree
            ssh_ok(), // delete branch
            ssh_ok(), // rm state file
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
        let calls = ssh.calls();
        assert_eq!(
            calls.last().unwrap(),
            "rm -f ~/.skulk/state/skulk-target",
            "last call should be the state-file rm, got: {calls:?}"
        );
    }

    #[test]
    fn cmd_destroy_not_found_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let result = cmd_destroy(&ssh, "ghost", true, &cfg, &confirm_yes);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"));
        });
    }

    #[test]
    fn cmd_destroy_aborted_by_user() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory_single_agent("skulk-target"))]);
        assert!(cmd_destroy(&ssh, "target", false, &cfg, &confirm_no).is_ok());
    }

    #[test]
    fn cmd_destroy_confirmed_by_user() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(), // kill session
            ssh_ok(), // worktree remove
            ssh_ok(), // branch delete
            ssh_ok(), // rm state file
        ]);
        assert!(cmd_destroy(&ssh, "target", false, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_session_only() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-partial"], &[], &["skulk-partial"])),
            ssh_ok(), // kill session
            ssh_ok(), // orphan-branch delete
            ssh_ok(), // rm state file
        ]);
        assert!(cmd_destroy(&ssh, "partial", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_session_destroy_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_err("kill-session failed"),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(), // rm state file still runs
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_worktree_destroy_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(),
            ssh_err("worktree remove failed"),
            ssh_ok(),
            ssh_ok(), // rm state file still runs
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_branch_destroy_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(),
            ssh_ok(),
            ssh_err("branch delete failed"),
            ssh_ok(), // rm state file still runs
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_steps_fail() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_err("kill failed"),
            ssh_err("worktree failed"),
            ssh_err("branch failed"),
            ssh_err("state rm failed"), // rm state file still attempted, failure ignored
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_orphaned_branch_only() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-orphan"])),
            ssh_ok(), // branch delete
            ssh_ok(), // rm state file
        ]);
        assert!(cmd_destroy(&ssh, "orphan", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_orphaned_branch_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-orphan"])),
            ssh_err("branch delete failed"),
            ssh_ok(), // rm state file still runs
        ]);
        assert!(cmd_destroy(&ssh, "orphan", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_issues_state_file_rm() {
        // End-to-end check that the state-file rm is actually issued, not just
        // that the test mock accepts an extra response.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
        assert!(
            ssh.calls()
                .iter()
                .any(|c| c == "rm -f ~/.skulk/state/skulk-target"),
            "expected rm of state file, got calls: {:?}",
            ssh.calls()
        );
    }

    #[test]
    fn cmd_destroy_all_empty_inventory() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_aborted_by_user() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory_single_agent("skulk-alpha"))]);
        assert!(cmd_destroy_all(&ssh, false, &cfg, &confirm_no).is_ok());
    }

    #[test]
    fn cmd_destroy_all_with_agents() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-alpha", "skulk-beta"],
                &[
                    ("skulk-alpha", "/path/skulk-alpha"),
                    ("skulk-beta", "/path/skulk-beta"),
                ],
                &["skulk-alpha", "skulk-beta"],
            )),
            // alpha: kill, wt, br, state rm
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            // beta: kill, wt, br, state rm
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "rm -f ~/.skulk/state/skulk-alpha"),
            "expected rm for alpha state file: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c == "rm -f ~/.skulk/state/skulk-beta"),
            "expected rm for beta state file: {calls:?}"
        );
    }

    #[test]
    fn cmd_destroy_all_some_steps_fail() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-alpha")),
            ssh_err("session kill failed"),
            ssh_err("worktree remove failed"),
            ssh_err("branch delete failed"),
            ssh_err("state rm failed"), // state rm still runs, failure ignored
        ]);
        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_mixed_success_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-alpha", "skulk-beta"],
                &[
                    ("skulk-alpha", "/path/skulk-alpha"),
                    ("skulk-beta", "/path/skulk-beta"),
                ],
                &["skulk-alpha", "skulk-beta"],
            )),
            // alpha: kill ok, wt ok, br ok, state rm ok
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            // beta: kill fails, wt ok, br fails, state rm ok
            ssh_err("session kill failed"),
            ssh_ok(),
            ssh_err("branch delete failed"),
            ssh_ok(),
        ]);
        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
    }
}
