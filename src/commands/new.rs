use std::path::{Path, PathBuf};

use crate::agent_ref::AgentRef;
use crate::commands::prompt_source;
use crate::config::{Config, DEFAULT_HARNESS};
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::fetch_inventory;
use crate::remote::{self, LaunchOptions, STARTUP_DELAY};
use crate::ssh::Ssh;
use crate::util::{check_base_clone, validate_model, validate_name};

/// Result of delivering a prompt to a newly-created agent.
enum PromptStatus {
    Delivered,
    Failed,
    NotSent,
}

/// Resolve the local `.skulk/.env` path for the current project, if it exists.
///
/// Uses `cfg.root_dir` (the project directory containing `.skulk/`) to locate
/// `<root_dir>/.skulk/.env`. Returns `None` if the config was built without a
/// root dir or the env file isn't present on disk.
pub(crate) fn resolve_local_env_file(cfg: &Config) -> Option<PathBuf> {
    let root = cfg.root_dir.as_ref()?;
    let candidate = root.join(".skulk").join(".env");
    candidate.is_file().then_some(candidate)
}

/// Create a new agent with worktree isolation and optional initial prompt.
///
/// Orchestration sequence:
/// 1. Resolve the initial prompt from --github or --from (mutually exclusive)
/// 2. Validate name
/// 3. Check base clone exists
/// 4. Fetch inventory and check uniqueness
/// 5. Create worktree
/// 6. Upload local `.skulk/.env` to the worktree if present
///    - On failure: warn user, continue (init.sh runs without sourced vars)
/// 7. Create tmux session with Claude Code (with `--remote-control` if requested)
///    - On failure: rollback worktree
/// 8. Persist the prompt to `~/.skulk/prompts/<session>.txt` for `skulk replay`
///    - On failure: warn user, keep agent alive (replay just won't work)
/// 9. Send prompt if provided
///    - On failure: warn user, keep agent alive
/// 10. Print success output
//
// Explicitly allow `too_many_arguments` / `too_many_lines`: `cmd_new` is the
// top-level orchestrator for the `new` command, and each argument is a distinct
// externally-supplied input required by one of the steps above. Grouping
// them into a struct or splitting the function would hide the linear sequence
// that makes this function easy to follow.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_new(
    ssh: &impl Ssh,
    name: &str,
    github: Option<&str>,
    from: Option<&Path>,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
    cfg: &Config,
    local_env_file: Option<&Path>,
) -> Result<(), SkulkError> {
    let agent = AgentRef::new(name, cfg);
    let branch = agent.branch_name();

    // Step 1: Resolve initial prompt (if any). clap `conflicts_with` on the CLI
    // enforces that both --github and --from can't be set; the unreachable arm
    // is a defensive guard for non-CLI callers.
    let prompt: Option<String> = match (github, from) {
        (None, None) => None,
        (Some(id), None) => Some(prompt_source::load_github_prompt(ssh, id, &branch, cfg)?),
        (None, Some(path)) => Some(prompt_source::load_file_prompt(path, &branch)?),
        (Some(_), Some(_)) => unreachable!("--github and --from are mutually exclusive"),
    };

    create_agent_with_prompt(
        ssh,
        name,
        prompt.as_deref(),
        remote_control,
        model,
        claude_args,
        cfg,
        local_env_file,
    )
}

/// Shared creation pipeline: accepts a pre-resolved prompt string (already
/// wrapped in whatever task-assignment boilerplate it needs) and runs every
/// side-effecting step of `cmd_new` from name validation through to the
/// final success banner.
///
/// Used by both `cmd_new` (after resolving `--github`/`--from`) and
/// `cmd_replay` (with the prompt read back from the remote store). Exposed
/// so `replay` can skip the `--github`/`--from` resolution — its input is
/// already a fully-formed prompt.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn create_agent_with_prompt(
    ssh: &impl Ssh,
    name: &str,
    prompt: Option<&str>,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
    cfg: &Config,
    local_env_file: Option<&Path>,
) -> Result<(), SkulkError> {
    let base_path = &cfg.base_path;
    let host = &cfg.host;
    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let branch = agent.branch_name();
    let worktree = agent.worktree_path(cfg);

    // Step 2: Validate name and (if provided) model
    validate_name(name)?;
    if let Some(m) = model {
        validate_model(m)?;
    }

    // Step 1: Check base clone exists
    check_base_clone(ssh, cfg, || {
        format!(
            "Base clone not found at {base_path} on {host}. Run `skulk pull` or clone manually."
        )
    })?;

    // Step 2: Fetch inventory and check uniqueness
    let inv = fetch_inventory(ssh, cfg).map_err(|e| classify_agent_error(name, e, host))?;
    inv.lifecycle(&AgentRef::new(name, cfg))
        .ensure_name_free(name)?;

    // Step 3: Create worktree
    remote::create_worktree(ssh, name, cfg)?;

    // Step 4: Upload local .env to worktree if present.
    // Non-fatal: if the copy fails, init.sh still runs but without the sourced vars.
    if let Some(env_file) = local_env_file {
        let remote_env = format!("{worktree}/.env");
        if let Err(e) = ssh.upload_file(env_file, &remote_env) {
            eprintln!(
                "Warning: failed to copy {} to agent worktree: {e}",
                env_file.display()
            );
        }
    }

    // Step 5: Create tmux session with Claude Code
    let launch = LaunchOptions {
        remote_control,
        model,
        claude_args,
    };
    if let Err(failure) = remote::start_session_or_kill(ssh, name, cfg, &launch) {
        if failure.cleanup_failed {
            eprintln!(
                "Warning: failed to clean up partial tmux session for agent '{name}'. \
                 Run `skulk destroy {name}` to remove it."
            );
        }
        // Attempt rollback (best-effort)
        remote::roll_back_worktree(ssh, name, cfg);
        return Err(failure.error);
    }

    // Step 5.5: Update the persisted prompt file so `skulk replay <name>`
    // reflects this incarnation. With a prompt we write it (for future
    // replays); without one we wipe any file left over from a prior
    // incarnation — otherwise `skulk replay` would silently serve the old
    // prompt against an unrelated new agent. Runs *before* send so the
    // prompt is durable even if delivery fails. Non-fatal either way: the
    // agent is already running.
    let stored = match prompt {
        Some(p) => remote::persist_prompt(ssh, &session_name, p),
        None => remote::clear_prompt(ssh, &session_name),
    };
    if let Err(e) = stored {
        eprintln!("Warning: failed to update stored prompt for agent '{name}': {e}");
    }

    // Step 6: Send prompt if provided
    let prompt_status = if let Some(prompt_text) = prompt {
        if remote::send_initial_prompt(ssh, name, prompt_text, cfg).is_ok() {
            PromptStatus::Delivered
        } else {
            eprintln!(
                "Warning: Agent '{name}' created but prompt delivery failed. Send manually:\n  skulk send {name} \"...\""
            );
            PromptStatus::Failed
        }
    } else {
        PromptStatus::NotSent
    };

    // Step 7: Print success output
    let prompt_line = match prompt_status {
        PromptStatus::Delivered => format!("  Prompt: delivered (after {STARTUP_DELAY}s delay)"),
        PromptStatus::Failed => {
            "  Prompt: delivery failed (agent is running, send manually)".to_string()
        }
        PromptStatus::NotSent => "  Prompt: none (idle, waiting for input)".to_string(),
    };

    // For Claude, --dangerously-skip-permissions and --remote-control land on
    // the launch line. For OpenCode, neither flag exists on the TUI: permission
    // is granted via opencode.json, and --remote-control is silently dropped
    // (with a stderr warning) by build_launch_sequence. The banner reflects
    // what actually took effect, not what the user asked for.
    let is_claude = cfg.harness == DEFAULT_HARNESS;
    let permissions_line = match (is_claude, remote_control) {
        (true, true) => "  Mode: remote-control (skip-permissions)".to_string(),
        (true, false) => "  Mode: skip-permissions".to_string(),
        (false, _) => "  Mode: permission-allow (via opencode.json)".to_string(),
    };

    let harness_line = format!("\n  Harness: {}", cfg.harness);
    let model_line = model.map(|m| format!("\n  Model: {m}")).unwrap_or_default();
    let extra_args_line = claude_args
        .filter(|s| !s.is_empty())
        .map(|a| format!("\n  Extra Claude args: {a}"))
        .unwrap_or_default();

    println!(
        "Agent '{name}' created.\n\
         \x20 Branch: {branch}\n\
         \x20 Worktree: {worktree}\n\
         {permissions_line}{harness_line}{model_line}{extra_args_line}\n\
         {prompt_line}\n\
         \n\
         Next steps:\n\
         \x20 skulk connect {name}    # attach to session\n\
         \x20 skulk send {name} \"...\" # send a prompt\n\
         \x20 skulk destroy {name}    # tear down"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, mock_empty_inventory, mock_inventory, mock_inventory_single_agent,
        ssh_ok, test_config,
    };

    #[test]
    fn cmd_new_persists_prompt_after_tmux_create_before_send() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // persist prompt
            ssh_ok(), // send prompt
        ]);
        let prompt_file = std::env::temp_dir().join("skulk_cmd_new_persists_prompt.txt");
        std::fs::write(&prompt_file, "fix the bug").unwrap();
        let result = cmd_new(
            &ssh,
            "task",
            None,
            Some(&prompt_file),
            false,
            None,
            None,
            &cfg,
            None,
        );
        let _ = std::fs::remove_file(&prompt_file);
        assert!(result.is_ok());
        let calls = ssh.calls();
        // Persist step lands between tmux-create (3) and send (5).
        let persist_call = &calls[4];
        assert!(
            persist_call.contains("~/.skulk/prompts/skulk-task.txt"),
            "persist step should target prompts dir, got: {persist_call}"
        );
        assert!(
            persist_call.contains("printf '%s'"),
            "persist step should use printf '%s', got: {persist_call}"
        );
        // send step follows.
        assert!(
            calls[5].contains("tmux paste-buffer"),
            "send step should follow persist, got: {}",
            calls[5]
        );
    }

    #[test]
    fn cmd_new_without_prompt_clears_stored_prompt() {
        // Prompt files deliberately survive `skulk destroy` so `skulk replay`
        // keeps working after a teardown. The flip side: if you then create
        // a same-named agent without a prompt, any file left from the prior
        // incarnation would silently serve as its "original" prompt. cmd_new
        // therefore issues a `rm -f` of the stored file when no prompt is
        // given, so replay of the new incarnation correctly reports "no
        // stored prompt".
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale prompt
        ]);
        assert!(cmd_new(&ssh, "task", None, None, false, None, None, &cfg, None).is_ok());
        let calls = ssh.calls();
        let clear_call = calls
            .iter()
            .find(|c| c.contains("~/.skulk/prompts/skulk-task.txt"))
            .unwrap_or_else(|| panic!("expected a clear call for the stored prompt: {calls:?}"));
        assert!(
            clear_call.contains("rm -f"),
            "clear step should use `rm -f` (missing file is the common case): {clear_call}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("printf") && c.contains("~/.skulk/prompts/")),
            "no persist-prompt call expected when no prompt: {calls:?}"
        );
    }

    #[test]
    fn cmd_new_persist_failure_does_not_abort() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),                                       // worktree
            ssh_ok(),                                       // tmux create
            Err(SkulkError::SshFailed("disk full".into())), // persist fails
            ssh_ok(),                                       // send still runs
        ]);
        let prompt_file = std::env::temp_dir().join("skulk_cmd_new_persist_fail.txt");
        std::fs::write(&prompt_file, "fix it").unwrap();
        let result = cmd_new(
            &ssh,
            "task",
            None,
            Some(&prompt_file),
            false,
            None,
            None,
            &cfg,
            None,
        );
        let _ = std::fs::remove_file(&prompt_file);
        assert!(result.is_ok(), "persist failure must not abort cmd_new");
    }

    #[test]
    fn cmd_new_succeeds_without_prompt() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt
        ]);
        assert!(cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None).is_ok());
    }

    #[test]
    fn cmd_new_succeeds_with_prompt() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // persist prompt
            ssh_ok(), // send prompt
        ]);
        let prompt_file = std::env::temp_dir().join("skulk_cmd_new_succeeds_with_prompt.txt");
        std::fs::write(&prompt_file, "fix the bug").unwrap();
        let result = cmd_new(
            &ssh,
            "test",
            None,
            Some(&prompt_file),
            false,
            None,
            None,
            &cfg,
            None,
        );
        let _ = std::fs::remove_file(&prompt_file);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_new_with_remote_control_flag_passes_flag_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt
        ]);
        assert!(cmd_new(&ssh, "test", None, None, true, None, None, &cfg, None).is_ok());
        // Fourth SSH call is the tmux-create command; verify the flag landed there.
        let tmux_call = &ssh.calls()[3];
        assert!(
            tmux_call.contains("--remote-control skulk-test"),
            "tmux launch command should include --remote-control when flag is true, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_new_without_remote_control_flag_omits_flag() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt
        ]);
        assert!(cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None).is_ok());
        let tmux_call = &ssh.calls()[3];
        assert!(
            !tmux_call.contains("--remote-control"),
            "tmux launch command should omit --remote-control when flag is false, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_new_with_model_flag_passes_flag_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt
        ]);
        assert!(
            cmd_new(
                &ssh,
                "test",
                None,
                None,
                false,
                Some("opus"),
                None,
                &cfg,
                None
            )
            .is_ok()
        );
        let tmux_call = &ssh.calls()[3];
        assert!(
            tmux_call.contains("--model opus"),
            "tmux launch command should include --model opus, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_new_rejects_invalid_model_before_ssh() {
        let cfg = test_config();
        // No SSH responses queued — validation must fail before any SSH call.
        let ssh = MockSsh::new(vec![]);
        let result = cmd_new(
            &ssh,
            "test",
            None,
            None,
            false,
            Some("opus; rm -rf /"),
            None,
            &cfg,
            None,
        );
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("Invalid character"));
        });
        assert!(
            ssh.calls().is_empty(),
            "validation must short-circuit before any SSH call, got: {:?}",
            ssh.calls()
        );
    }

    #[test]
    fn cmd_new_with_claude_args_passes_args_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt
        ]);
        assert!(
            cmd_new(
                &ssh,
                "test",
                None,
                None,
                false,
                None,
                Some("--allowed-tools Bash"),
                &cfg,
                None,
            )
            .is_ok()
        );
        let tmux_call = &ssh.calls()[3];
        assert!(
            tmux_call.contains("--allowed-tools Bash"),
            "tmux launch command should include extra claude_args, got: {tmux_call}"
        );
    }

    #[test]
    fn cmd_new_duplicate_agent_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory_single_agent("skulk-dupe")),
        ]);
        let result = cmd_new(&ssh, "dupe", None, None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("already exists"));
        });
    }

    #[test]
    fn cmd_new_existing_worktree_suggests_restart_or_destroy() {
        let cfg = test_config();
        // No session, but worktree exists -- archived agent or crashed `new`.
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(
                &[],
                &[("skulk-zombie", "/path/skulk-zombie")],
                &["skulk-zombie"],
            )),
        ]);
        let result = cmd_new(&ssh, "zombie", None, None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(
                msg.contains("skulk restart zombie"),
                "should suggest restart: {msg}"
            );
            assert!(
                msg.contains("skulk destroy zombie"),
                "should suggest destroy: {msg}"
            );
        });
    }

    #[test]
    fn cmd_new_missing_base_clone_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("test failed".into()))]);
        let result = cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("Base clone not found"));
        });
    }

    #[test]
    fn cmd_new_tmux_failure_rolls_back() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            Err(SkulkError::SshFailed("tmux failed".into())),
            ssh_ok(), // kill partial session
            ssh_ok(), // rollback worktree
        ]);
        let result = cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None);
        assert!(result.is_err());
    }

    #[test]
    fn cmd_new_tmux_failure_kills_partial_session_before_rolling_back_worktree() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            Err(SkulkError::SshFailed("send-keys failed".into())),
            ssh_ok(),
            ssh_ok(),
        ]);

        let result = cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None);

        assert!(result.is_err());
        let calls = ssh.calls();
        assert_eq!(calls.len(), 6, "got: {calls:?}");
        assert!(calls[4].contains("tmux kill-session -t skulk-test"));
        assert!(calls[5].contains("git worktree remove --force"));
    }

    #[test]
    fn cmd_new_tmux_fails_rollback_also_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            Err(SkulkError::SshFailed("tmux creation failed".into())),
            ssh_ok(), // kill partial session
            Err(SkulkError::SshFailed("rollback also failed".into())),
        ]);
        let result = cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::SshFailed(msg) => {
            assert!(msg.contains("tmux creation failed"));
        });
    }

    #[test]
    fn cmd_new_prompt_delivery_fails_still_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // persist prompt
            Err(SkulkError::SshFailed("send-keys failed".into())),
        ]);
        let prompt_file = std::env::temp_dir().join("skulk_cmd_new_prompt_delivery_fails.txt");
        std::fs::write(&prompt_file, "fix the bug").unwrap();
        let result = cmd_new(
            &ssh,
            "test",
            None,
            Some(&prompt_file),
            false,
            None,
            None,
            &cfg,
            None,
        );
        let _ = std::fs::remove_file(&prompt_file);
        assert!(result.is_ok());
    }

    // ── cmd_new .env upload behavior ───────────────────────────────────

    #[test]
    fn cmd_new_uploads_local_env_when_provided() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt (no prompt provided)
        ]);
        let env_path = std::path::PathBuf::from("/tmp/some/.skulk/.env");
        assert!(
            cmd_new(
                &ssh,
                "test",
                None,
                None,
                false,
                None,
                None,
                &cfg,
                Some(&env_path)
            )
            .is_ok()
        );
        let calls = ssh.calls();
        // Order: base-clone check, inventory, worktree, UPLOAD, tmux
        let upload_call = calls
            .iter()
            .find(|c| c.starts_with("UPLOAD "))
            .expect("expected an UPLOAD call when local_env_file is provided");
        assert!(upload_call.contains("/tmp/some/.skulk/.env"));
        assert!(upload_call.contains("~/test-project-worktrees/skulk-test/.env"));
    }

    #[test]
    fn cmd_new_skips_upload_when_no_local_env() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt
        ]);
        assert!(cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None).is_ok());
        let calls = ssh.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("UPLOAD ")),
            "no upload expected when local_env_file is None: {calls:?}"
        );
    }

    #[test]
    fn cmd_new_upload_failure_is_non_fatal() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create
            ssh_ok(), // clear stale stored prompt
        ])
        .with_upload_responses(vec![Err(SkulkError::SshFailed("scp blew up".into()))]);
        let env_path = std::path::PathBuf::from("/tmp/.skulk/.env");
        // Upload failure should warn but still succeed overall.
        assert!(
            cmd_new(
                &ssh,
                "test",
                None,
                None,
                false,
                None,
                None,
                &cfg,
                Some(&env_path)
            )
            .is_ok()
        );
    }

    #[test]
    fn resolve_local_env_file_finds_existing_env() {
        let dir = std::env::temp_dir().join("skulk_env_resolve_yes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".skulk")).unwrap();
        std::fs::write(dir.join(".skulk").join(".env"), "FOO=bar\n").unwrap();

        let mut cfg = test_config();
        cfg.root_dir = Some(dir.clone());
        let resolved = resolve_local_env_file(&cfg);
        assert_eq!(
            resolved.as_deref(),
            Some(dir.join(".skulk").join(".env").as_path())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_local_env_file_returns_none_when_missing() {
        let dir = std::env::temp_dir().join("skulk_env_resolve_no");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut cfg = test_config();
        cfg.root_dir = Some(dir.clone());
        assert!(resolve_local_env_file(&cfg).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_local_env_file_returns_none_without_root_dir() {
        let cfg = test_config();
        assert!(resolve_local_env_file(&cfg).is_none());
    }

    #[test]
    fn cmd_new_base_clone_check_connectivity_error_propagated() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection refused.".into(),
            suggestion: "SSH not running.".into(),
        })]);
        let result = cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.contains("refused"));
        });
    }

    // ── --github partial-failure guarantees ────────────────────────────
    //
    // `load_github_prompt` runs before any mutating SSH call (worktree, tmux).
    // If the gh availability check or issue fetch fails, cmd_new must return
    // the error without creating any agent resources -- i.e., no cleanup is
    // needed because no state was committed. These tests pin that invariant.

    #[test]
    fn cmd_new_github_gh_unavailable_creates_no_resources() {
        let cfg = test_config();
        // First SSH call is the gh availability probe; simulate gh missing.
        let ssh = MockSsh::new(vec![Ok("SKULK_GH_MISSING".into())]);
        let result = cmd_new(
            &ssh,
            "test",
            Some("42"),
            None,
            false,
            None,
            None,
            &cfg,
            None,
        );
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.contains("not installed"));
        });
        // Only the gh availability check ran -- no worktree or tmux commands.
        let calls = ssh.calls();
        assert_eq!(calls.len(), 1, "only gh probe should have run: {calls:?}");
        assert!(
            !calls.iter().any(|c| c.contains("git worktree add")),
            "worktree creation must not run: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("tmux new-session")),
            "tmux creation must not run: {calls:?}"
        );
    }

    #[test]
    fn cmd_new_github_issue_fetch_fails_creates_no_resources() {
        let cfg = test_config();
        // gh is available, but the issue fetch fails -- still returns before
        // any mutating SSH call, so nothing to roll back.
        let ssh = MockSsh::new(vec![
            Ok("SKULK_GH_OK".into()),
            Err(SkulkError::SshFailed(
                "Could not resolve to an Issue with the number of 999.".into(),
            )),
        ]);
        let result = cmd_new(
            &ssh,
            "test",
            Some("999"),
            None,
            false,
            None,
            None,
            &cfg,
            None,
        );
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("#999"));
        });
        let calls = ssh.calls();
        assert_eq!(
            calls.len(),
            2,
            "only gh probe + fetch should have run: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("git worktree add")),
            "worktree creation must not run: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("tmux new-session")),
            "tmux creation must not run: {calls:?}"
        );
    }
}
