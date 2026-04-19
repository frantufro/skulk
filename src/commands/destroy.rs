use std::collections::HashSet;

use crate::config::Config;
use crate::error::SkulkError;
use crate::inventory::{inventory_command, parse_inventory};
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command to kill a tmux session for an agent.
pub(crate) fn agent_destroy_session_command(name: &str, cfg: &Config) -> String {
    let session_prefix = &cfg.session_prefix;
    format!("tmux kill-session -t {session_prefix}{name}")
}

/// Build the SSH command to remove an agent's git worktree.
pub(crate) fn agent_destroy_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let session_prefix = &cfg.session_prefix;
    let worktree_base = &cfg.worktree_base;
    format!("cd {base_path} && git worktree remove --force {worktree_base}/{session_prefix}{name}")
}

/// Build the SSH command to delete an agent's git branch.
pub(crate) fn agent_destroy_branch_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let session_prefix = &cfg.session_prefix;
    format!("cd {base_path} && git branch -D {session_prefix}{name}")
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

    let session_prefix = &cfg.session_prefix;

    // Fetch inventory via shared probe
    let inv = parse_inventory(&ssh.run(&inventory_command(cfg))?, cfg);
    let session_name = format!("{session_prefix}{name}");
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
    let session_prefix = &cfg.session_prefix;
    let inv = parse_inventory(&ssh.run(&inventory_command(cfg))?, cfg);

    // Build comprehensive agent name set
    let mut name_set: HashSet<String> = HashSet::new();
    for s in &inv.sessions {
        if let Some(name) = s.strip_prefix(&**session_prefix) {
            name_set.insert(name.to_string());
        }
    }
    for key in inv.worktrees.keys() {
        if let Some(name) = key.strip_prefix(&**session_prefix) {
            name_set.insert(name.to_string());
        }
    }
    for b in &inv.branches {
        if let Some(name) = b.strip_prefix(&**session_prefix) {
            name_set.insert(name.to_string());
        }
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
        print!("Destroying {session_prefix}{name}... ");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        let session_name = format!("{session_prefix}{name}");
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
    use crate::testutil::{MockSsh, mock_inventory, test_config};

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
            Ok(mock_inventory(
                &["skulk-target"],
                &[("skulk-target", "/path/skulk-target")],
                &["skulk-target"],
            )),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_not_found_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(&[], &[], &[]))]);
        let result = cmd_destroy(&ssh, "ghost", true, &cfg, &confirm_yes);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::NotFound(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn cmd_destroy_aborted_by_user() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(
            &["skulk-target"],
            &[("skulk-target", "/path/skulk-target")],
            &["skulk-target"],
        ))]);
        assert!(cmd_destroy(&ssh, "target", false, &cfg, &confirm_no).is_ok());
    }

    #[test]
    fn cmd_destroy_confirmed_by_user() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-target"],
                &[("skulk-target", "/path/skulk-target")],
                &["skulk-target"],
            )),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_destroy(&ssh, "target", false, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_session_only() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&["skulk-partial"], &[], &["skulk-partial"])),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_destroy(&ssh, "partial", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_session_destroy_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-target"],
                &[("skulk-target", "/path/skulk-target")],
                &["skulk-target"],
            )),
            Err(SkulkError::SshFailed("kill-session failed".into())),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_worktree_destroy_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-target"],
                &[("skulk-target", "/path/skulk-target")],
                &["skulk-target"],
            )),
            Ok(String::new()),
            Err(SkulkError::SshFailed("worktree remove failed".into())),
            Ok(String::new()),
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_branch_destroy_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-target"],
                &[("skulk-target", "/path/skulk-target")],
                &["skulk-target"],
            )),
            Ok(String::new()),
            Ok(String::new()),
            Err(SkulkError::SshFailed("branch delete failed".into())),
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_steps_fail() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-target"],
                &[("skulk-target", "/path/skulk-target")],
                &["skulk-target"],
            )),
            Err(SkulkError::SshFailed("kill failed".into())),
            Err(SkulkError::SshFailed("worktree failed".into())),
            Err(SkulkError::SshFailed("branch failed".into())),
        ]);
        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_orphaned_branch_only() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-orphan"])),
            Ok(String::new()),
        ]);
        assert!(cmd_destroy(&ssh, "orphan", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_orphaned_branch_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(&[], &[], &["skulk-orphan"])),
            Err(SkulkError::SshFailed("branch delete failed".into())),
        ]);
        assert!(cmd_destroy(&ssh, "orphan", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_empty_inventory() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(&[], &[], &[]))]);
        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_aborted_by_user() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(
            &["skulk-alpha"],
            &[("skulk-alpha", "/path/skulk-alpha")],
            &["skulk-alpha"],
        ))]);
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
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_some_steps_fail() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &["skulk-alpha"],
                &[("skulk-alpha", "/path/skulk-alpha")],
                &["skulk-alpha"],
            )),
            Err(SkulkError::SshFailed("session kill failed".into())),
            Err(SkulkError::SshFailed("worktree remove failed".into())),
            Err(SkulkError::SshFailed("branch delete failed".into())),
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
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Err(SkulkError::SshFailed("session kill failed".into())),
            Ok(String::new()),
            Err(SkulkError::SshFailed("branch delete failed".into())),
        ]);
        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
    }
}
