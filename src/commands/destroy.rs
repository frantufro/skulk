use std::collections::HashSet;

use crate::agent::Agent;
use crate::agent_ref::AgentRef;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::SkulkError;
use crate::inventory::{Lifecycle, fetch_inventory};
use crate::remote;
use crate::ssh::Ssh;

/// A piece of an agent that `destroy` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resource {
    Session,
    Worktree,
    Branch,
}

impl Resource {
    /// Wording used in human-readable output.
    fn label(self) -> &'static str {
        match self {
            Resource::Session => "tmux session",
            Resource::Worktree => "worktree",
            Resource::Branch => "branch",
        }
    }

    /// Stable identifier used in JSON output.
    fn key(self) -> &'static str {
        match self {
            Resource::Session => "session",
            Resource::Worktree => "worktree",
            Resource::Branch => "branch",
        }
    }
}

/// What a single-agent destroy removed and what it failed to remove.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DestroyOutcome {
    pub(crate) cleaned: Vec<Resource>,
    pub(crate) failed: Vec<Resource>,
}

impl DestroyOutcome {
    fn record(&mut self, resource: Resource, succeeded: bool) {
        if succeeded {
            self.cleaned.push(resource);
        } else {
            self.failed.push(resource);
        }
    }
}

/// Build the error returned when one or more `destroy` steps failed.
///
/// Points the user at `skulk destroy <name>` (not `skulk gc`): git refuses to
/// delete a branch while its worktree is still checked out, so a worktree-
/// remove failure guarantees the branch stays too — `skulk gc` classifies
/// that state as an *archived* agent and refuses to touch it, leaving the
/// user stuck. `skulk destroy <name>` re-fetches inventory and retries only
/// the steps that still have work to do.
pub(crate) fn partial_destroy_error(name: &str, failed: &[&str]) -> SkulkError {
    SkulkError::Diagnostic {
        message: format!(
            "Failed to clean up {} for agent '{name}'.",
            failed.join(", ")
        ),
        suggestion: format!("Retry with `skulk destroy {name}`."),
    }
}

/// Build the error returned by `destroy-all` when some agents left resources behind.
pub(crate) fn partial_destroy_all_error(clean_count: usize, failed: &[&str]) -> SkulkError {
    SkulkError::Diagnostic {
        message: format!(
            "Destroyed {clean_count} agent(s); {} left resources behind: {}.",
            failed.len(),
            failed.join(", ")
        ),
        suggestion:
            "Retry with `skulk destroy-all` or `skulk destroy <name>` for individual agents."
                .to_string(),
    }
}

/// JSON mode has no one to answer the `[y/N]` prompt, so it must be skipped
/// explicitly with `--force`.
fn require_force_for_json(force: bool, cfg: &Config) -> Result<(), SkulkError> {
    if cfg.output_format == OutputFormat::Json && !force {
        return Err(SkulkError::Validation(
            "--json requires --force: there is no confirmation prompt in JSON mode.".to_string(),
        ));
    }
    Ok(())
}

/// Destroy a specific agent (kills session, removes worktree, deletes branch).
///
/// Uses shared inventory to probe what exists.
/// Each cleanup step is independent -- if tmux is gone but worktree remains,
/// the worktree is still removed. Errors if nothing exists at all, or if any
/// step failed (after the other steps have run).
pub(crate) fn cmd_destroy(
    ssh: &impl Ssh,
    name: &str,
    force: bool,
    cfg: &Config,
    confirm: &dyn Fn(&str) -> bool,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    require_force_for_json(force, cfg)?;

    // Fetch inventory via shared probe
    let inv = fetch_inventory(ssh, cfg)?;
    let session_name = agent.session_name();
    let has_session = inv.sessions.contains(&session_name);
    let has_worktree = inv.worktrees.contains_key(&session_name);
    let has_branch = inv.branches.contains(&session_name);

    if agent.lifecycle(&inv) == Lifecycle::Absent {
        return Err(SkulkError::NotFound(format!("No agent '{name}' found.")));
    }

    if !force && !confirm(&format!("Destroy agent '{name}'? [y/N]")) {
        println!("Aborted.");
        return Ok(());
    }

    let mut outcome = DestroyOutcome::default();
    if has_session {
        outcome.record(
            Resource::Session,
            remote::kill_session(ssh, name, cfg).is_ok(),
        );
    }
    if has_worktree {
        outcome.record(
            Resource::Worktree,
            remote::remove_worktree(ssh, name, cfg).is_ok(),
        );
    }
    // A branch whose worktree is gone is an orphan and is deleted too.
    if has_worktree || has_branch {
        outcome.record(
            Resource::Branch,
            remote::delete_branch(ssh, name, cfg).is_ok(),
        );
    }

    // Always reap the Stop-hook state marker. It's invisible plumbing for
    // `skulk wait`, so it isn't part of the outcome; `rm -f` makes a missing
    // file a no-op anyway.
    let _ = remote::remove_state_file(ssh, name, cfg);

    let labels = |rs: &[Resource]| rs.iter().map(|r| r.label()).collect::<Vec<_>>();
    if cfg.output_format == OutputFormat::Human && !outcome.cleaned.is_empty() {
        println!(
            "Destroyed agent '{name}' ({}).",
            labels(&outcome.cleaned).join(", ")
        );
    }
    if !outcome.failed.is_empty() {
        return Err(partial_destroy_error(name, &labels(&outcome.failed)));
    }
    if cfg.output_format == OutputFormat::Json {
        emit_json(&json_destroy_result(name, &outcome))?;
    }
    Ok(())
}

/// Build the JSON result for a `destroy` that removed everything it found.
pub(crate) fn json_destroy_result(name: &str, outcome: &DestroyOutcome) -> serde_json::Value {
    let cleaned: Vec<&str> = outcome.cleaned.iter().map(|r| r.key()).collect();
    serde_json::json!({
        "ok": true,
        "agent": name,
        "cleaned": cleaned,
    })
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
    require_force_for_json(force, cfg)?;
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

    let json = cfg.output_format == OutputFormat::Json;
    if agent_names.is_empty() {
        if json {
            emit_json(&json_destroy_all_result(&[]))?;
        } else {
            println!("No agents to destroy.");
        }
        return Ok(());
    }

    if !force && !confirm(&format!("Destroy {} agent(s)? [y/N]", agent_names.len())) {
        println!("Aborted.");
        return Ok(());
    }

    let mut failed: Vec<&str> = Vec::new();
    for name in &agent_names {
        let agent = AgentRef::new(name, cfg);
        let session_name = agent.session_name();
        if !json {
            print!("Destroying {session_name}... ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }

        let mut step_failed = false;
        if inv.sessions.contains(&session_name) && remote::kill_session(ssh, name, cfg).is_err() {
            step_failed = true;
        }
        if inv.worktrees.contains_key(&session_name)
            && remote::remove_worktree(ssh, name, cfg).is_err()
        {
            step_failed = true;
        }
        if inv.branches.contains(&session_name) && remote::delete_branch(ssh, name, cfg).is_err() {
            step_failed = true;
        }

        // Reap the Stop-hook state marker for this agent. Fire-and-forget;
        // `rm -f` makes missing files a no-op and state cleanup is invisible
        // plumbing, not something worth surfacing in the summary.
        let _ = remote::remove_state_file(ssh, name, cfg);

        if !json {
            println!(
                "{}",
                if step_failed {
                    "done (with warnings)"
                } else {
                    "done"
                }
            );
        }
        if step_failed {
            failed.push(name);
        }
    }

    if !failed.is_empty() {
        return Err(partial_destroy_all_error(
            agent_names.len() - failed.len(),
            &failed,
        ));
    }
    if json {
        emit_json(&json_destroy_all_result(&agent_names))?;
    } else {
        println!("Destroyed {} agent(s).", agent_names.len());
    }
    Ok(())
}

/// Build the JSON result for a `destroy-all` that removed every agent.
pub(crate) fn json_destroy_all_result(agents: &[String]) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "agents": agents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, mock_empty_inventory, mock_inventory, mock_inventory_single_agent,
        ssh_err, ssh_ok, test_config, test_config_json,
    };

    // ── partial-failure messaging ──────────────────────────────────────
    //
    // The original advice pointed users at `skulk gc`, but gc treats
    // (worktree + branch) as an archived agent and refuses to touch it.
    // When `git worktree remove` fails, the branch cannot be deleted
    // either (git won't drop a checked-out branch), so the user ends up
    // in exactly that gc-ignored state. These tests pin the corrected
    // advice so it can't silently regress.

    #[test]
    fn partial_destroy_error_points_at_destroy_not_gc() {
        let msg = partial_destroy_error("my-task", &["worktree", "branch"]).to_string();
        assert!(
            msg.contains("worktree, branch"),
            "should list failed items: {msg}"
        );
        assert!(msg.contains("my-task"), "should name the agent: {msg}");
        assert!(
            msg.contains("skulk destroy my-task"),
            "must tell user to retry with skulk destroy, got: {msg}"
        );
        assert!(
            !msg.contains("skulk gc"),
            "must NOT suggest skulk gc (it won't clean worktree+branch state): {msg}"
        );
    }

    #[test]
    fn partial_destroy_all_error_counts_and_names_leftovers() {
        let msg = partial_destroy_all_error(2, &["b"]).to_string();
        assert!(msg.contains("2 agent(s)"), "should count clean: {msg}");
        assert!(
            msg.contains("1 left resources behind: b"),
            "should name leftovers: {msg}"
        );
        assert!(
            msg.contains("skulk destroy-all") || msg.contains("skulk destroy"),
            "must tell user how to retry: {msg}"
        );
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
    fn json_destroy_result_reports_cleaned_resources_by_key() {
        let outcome = DestroyOutcome {
            cleaned: vec![Resource::Session, Resource::Worktree, Resource::Branch],
            failed: vec![],
        };

        let value = json_destroy_result("target", &outcome);

        assert_eq!(value["ok"], true);
        assert_eq!(value["agent"], "target");
        assert_eq!(
            value["cleaned"],
            serde_json::json!(["session", "worktree", "branch"])
        );
    }

    #[test]
    fn cmd_destroy_in_json_mode_requires_force_before_any_ssh() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![]);

        let result = cmd_destroy(&ssh, "target", false, &cfg, &confirm_yes);

        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("--force"), "{msg}");
        });
        assert_eq!(ssh.calls().len(), 0);
    }

    #[test]
    fn cmd_destroy_in_json_mode_returns_error_when_a_step_fails() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(),
            ssh_err("worktree failed"),
            ssh_err("branch failed"),
            ssh_ok(),
        ]);

        let result = cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes);

        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.contains("worktree, branch"), "{message}");
        });
    }

    #[test]
    fn cmd_destroy_in_json_mode_succeeds() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);

        assert!(cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_in_json_mode_returns_error_for_emit_json_error() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);

        let result = cmd_destroy(&ssh, "ghost", true, &cfg, &confirm_yes);

        assert_err!(result, SkulkError::NotFound(_) => {});
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
        assert!(matches!(
            cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes),
            Err(SkulkError::Diagnostic { .. })
        ));
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
        assert!(matches!(
            cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes),
            Err(SkulkError::Diagnostic { .. })
        ));
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
        assert!(matches!(
            cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes),
            Err(SkulkError::Diagnostic { .. })
        ));
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
        assert!(matches!(
            cmd_destroy(&ssh, "target", true, &cfg, &confirm_yes),
            Err(SkulkError::Diagnostic { .. })
        ));
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
        assert!(matches!(
            cmd_destroy(&ssh, "orphan", true, &cfg, &confirm_yes),
            Err(SkulkError::Diagnostic { .. })
        ));
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
    fn json_destroy_all_result_lists_each_agent() {
        let value = json_destroy_all_result(&["a".to_string(), "b".to_string()]);

        assert_eq!(value["ok"], true);
        assert_eq!(value["agents"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn json_destroy_all_result_with_nothing_to_destroy_is_ok() {
        let value = json_destroy_all_result(&[]);

        assert_eq!(value["ok"], true);
        assert_eq!(value["agents"], serde_json::json!([]));
    }

    #[test]
    fn cmd_destroy_all_in_json_mode_requires_force_before_any_ssh() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![]);

        let result = cmd_destroy_all(&ssh, false, &cfg, &confirm_yes);

        assert!(matches!(result, Err(SkulkError::Validation(_))));
        assert_eq!(ssh.calls().len(), 0);
    }

    #[test]
    fn cmd_destroy_all_in_json_mode_succeeds() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);

        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_ok());
    }

    #[test]
    fn cmd_destroy_all_in_json_mode_returns_error_for_emit_json_error() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![ssh_err("ssh: connect to host testhost: refused")]);

        assert!(cmd_destroy_all(&ssh, true, &cfg, &confirm_yes).is_err());
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
        assert!(matches!(
            cmd_destroy_all(&ssh, true, &cfg, &confirm_yes),
            Err(SkulkError::Diagnostic { .. })
        ));
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
        assert!(matches!(
            cmd_destroy_all(&ssh, true, &cfg, &confirm_yes),
            Err(SkulkError::Diagnostic { .. })
        ));
    }
}
