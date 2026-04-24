use std::collections::HashSet;
use std::path::Path;

use crate::agent_ref::AgentRef;
use crate::commands::new::create_agent_with_prompt;
use crate::config::Config;
use crate::error::SkulkError;
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command that reads an agent's persisted prompt.
///
/// `test -f … && cat …` is chosen so a missing file cleanly produces an SSH
/// failure (translated to `NotFound` by [`read_stored_prompt`]), while a
/// present file is read verbatim. The write side uses `printf '%s'` so no
/// trailing newline is added, and the SSH layer trims whitespace, so what
/// comes back matches what was originally sent to Claude.
pub(crate) fn agent_read_prompt_command(session_name: &str) -> String {
    format!(
        "test -f ~/.skulk/prompts/{session_name}.txt && cat ~/.skulk/prompts/{session_name}.txt"
    )
}

/// Read an agent's persisted prompt from the remote, mapping a missing file
/// (or empty stored file) to a user-friendly `NotFound`. `name` is the bare
/// agent name used in the error message; `session_name` is the prefixed
/// form used to locate the file on disk.
pub(crate) fn read_stored_prompt(
    ssh: &impl Ssh,
    name: &str,
    session_name: &str,
) -> Result<String, SkulkError> {
    match ssh.run(&agent_read_prompt_command(session_name)) {
        Ok(content) if content.is_empty() => Err(no_prompt_error(name)),
        Ok(content) => Ok(content),
        Err(SkulkError::SshFailed(_)) => Err(no_prompt_error(name)),
        Err(e) => Err(e),
    }
}

fn no_prompt_error(name: &str) -> SkulkError {
    SkulkError::NotFound(format!(
        "No stored prompt for '{name}'.\n  \
         Either it was created without a prompt, or with a version of skulk \
         that didn't persist prompts.\n  \
         Start a fresh agent with `skulk new` and pass --from or --github."
    ))
}

/// Split `name` on a trailing `-<digits>` suffix.
///
/// Returns `(base, next_n)` where `next_n` is the integer to start probing
/// from when deriving a replay name. `foo-bar-2` becomes `("foo-bar", 3)`;
/// `foo-bar` becomes `("foo-bar", 2)`.
fn split_numeric_suffix(name: &str) -> (&str, u32) {
    if let Some(dash_idx) = name.rfind('-') {
        let tail = &name[dash_idx + 1..];
        if !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
            && let Ok(n) = tail.parse::<u32>()
        {
            return (&name[..dash_idx], n.saturating_add(1));
        }
    }
    (name, 2)
}

/// Derive a free replay-agent name from `source_name` and the set of names
/// already taken on the remote.
///
/// Strategy: if `source_name` ends in `-<N>`, strip it and count up from
/// `N+1`; otherwise append `-2`, `-3`, … until an unused candidate is found.
/// Errors if no candidate fits under the 30-char agent-name limit.
pub(crate) fn derive_replay_name(
    source_name: &str,
    taken: &HashSet<String>,
) -> Result<String, SkulkError> {
    let (base, start) = split_numeric_suffix(source_name);
    let mut n = start;
    loop {
        let candidate = format!("{base}-{n}");
        if candidate.len() > 30 {
            return Err(SkulkError::Validation(format!(
                "Cannot derive a replay name under 30 chars from '{source_name}'. \
                 Pick one explicitly with --as <new-name>."
            )));
        }
        if !taken.contains(&candidate) {
            return Ok(candidate);
        }
        n = n
            .checked_add(1)
            .ok_or_else(|| SkulkError::Validation("Replay suffix overflowed u32.".into()))?;
    }
}

/// Re-run the original prompt of an existing agent on a fresh agent.
///
/// Reads the prompt persisted at `~/.skulk/prompts/<source_session>.txt` on
/// the remote (written by `cmd_new` at creation time), derives a new agent
/// name, and delegates to [`create_agent_with_prompt`] with the same prompt.
/// The new agent can run on a different `--model` or with different
/// `--claude-args`, which is the primary motivation for replay.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_replay(
    ssh: &impl Ssh,
    source_name: &str,
    new_name_override: Option<&str>,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
    cfg: &Config,
    local_env_file: Option<&Path>,
) -> Result<(), SkulkError> {
    validate_name(source_name)?;
    if let Some(n) = new_name_override {
        validate_name(n)?;
    }

    let source_session = AgentRef::new(source_name, cfg).session_name();
    let prompt = read_stored_prompt(ssh, source_name, &source_session)?;

    // Fetch inventory to pick a fresh name. `create_agent_with_prompt` runs
    // its own inventory probe for its uniqueness check — that's a second
    // round-trip but keeps the two responsibilities independent.
    let inv = fetch_inventory(ssh, cfg)?;
    let taken: HashSet<String> = inv
        .sessions
        .iter()
        .chain(inv.worktrees.keys())
        .chain(inv.branches.iter())
        .map(|q| AgentRef::from_qualified(q, cfg).name().to_string())
        .collect();

    let new_name = match new_name_override {
        Some(n) => {
            if taken.contains(n) {
                return Err(SkulkError::Validation(format!(
                    "Agent '{n}' already exists. Pick a different name or destroy it first."
                )));
            }
            n.to_string()
        }
        None => derive_replay_name(source_name, &taken)?,
    };

    eprintln!("Replaying prompt from '{source_name}' as '{new_name}'.");

    create_agent_with_prompt(
        ssh,
        &new_name,
        Some(&prompt),
        remote_control,
        model,
        claude_args,
        cfg,
        local_env_file,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, mock_empty_inventory, mock_inventory, mock_inventory_single_agent,
        ssh_ok, test_config,
    };

    // ── agent_read_prompt_command ────────────────────────────────────────

    #[test]
    fn agent_read_prompt_command_targets_prompts_dir() {
        let cmd = agent_read_prompt_command("skulk-task");
        assert!(cmd.contains("~/.skulk/prompts/skulk-task.txt"));
    }

    #[test]
    fn agent_read_prompt_command_gates_cat_on_test_f() {
        // The `test -f` gate is what makes a missing file surface as an SSH
        // failure the command layer can translate to NotFound. If someone
        // replaces this with a bare `cat`, missing prompts would spam the
        // user with a generic "No such file" SSH error.
        let cmd = agent_read_prompt_command("skulk-task");
        assert!(
            cmd.starts_with("test -f "),
            "test -f must come first: {cmd}"
        );
        assert!(
            cmd.contains(" && cat "),
            "cat must be gated on test -f: {cmd}"
        );
    }

    // ── read_stored_prompt ───────────────────────────────────────────────

    #[test]
    fn read_stored_prompt_returns_content_when_present() {
        let ssh = MockSsh::new(vec![Ok("the stored prompt".into())]);
        let out = read_stored_prompt(&ssh, "task", "skulk-task").unwrap();
        assert_eq!(out, "the stored prompt");
    }

    #[test]
    fn read_stored_prompt_preserves_multiline_content() {
        let ssh = MockSsh::new(vec![Ok("line one\nline two\nline three".into())]);
        let out = read_stored_prompt(&ssh, "task", "skulk-task").unwrap();
        assert_eq!(out, "line one\nline two\nline three");
    }

    #[test]
    fn read_stored_prompt_missing_file_returns_notfound() {
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("exit code 1".into()))]);
        let result = read_stored_prompt(&ssh, "task", "skulk-task");
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("task"), "error should name the agent: {msg}");
            assert!(msg.contains("skulk new"), "error should suggest next step: {msg}");
        });
    }

    #[test]
    fn read_stored_prompt_empty_file_returns_notfound() {
        // Defensive: an empty stored file behaves as if missing — the cmd_new
        // persist path only writes when a prompt is provided, so an empty
        // file means something went wrong and we should not feed empty
        // content to a fresh agent.
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        let result = read_stored_prompt(&ssh, "task", "skulk-task");
        assert_err!(result, SkulkError::NotFound(_) => {});
    }

    #[test]
    fn read_stored_prompt_propagates_non_sshfailed_errors() {
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection timed out.".into(),
            suggestion: "Check network.".into(),
        })]);
        let result = read_stored_prompt(&ssh, "task", "skulk-task");
        assert_err!(result, SkulkError::Diagnostic { .. } => {});
    }

    // ── split_numeric_suffix ─────────────────────────────────────────────

    #[test]
    fn split_numeric_suffix_no_suffix_defaults_to_two() {
        assert_eq!(split_numeric_suffix("auth-refactor"), ("auth-refactor", 2));
    }

    #[test]
    fn split_numeric_suffix_strips_and_increments() {
        assert_eq!(
            split_numeric_suffix("auth-refactor-2"),
            ("auth-refactor", 3)
        );
        assert_eq!(split_numeric_suffix("foo-99"), ("foo", 100));
    }

    #[test]
    fn split_numeric_suffix_ignores_non_numeric_tail() {
        assert_eq!(split_numeric_suffix("foo-bar"), ("foo-bar", 2));
        assert_eq!(split_numeric_suffix("foo-v2a"), ("foo-v2a", 2));
    }

    #[test]
    fn split_numeric_suffix_ignores_trailing_dash() {
        // `trailing-` would fail validate_name anyway, but guard against
        // panics if it ever reaches this helper from an unvalidated path.
        assert_eq!(split_numeric_suffix("trailing-"), ("trailing-", 2));
    }

    // ── derive_replay_name ───────────────────────────────────────────────

    #[test]
    fn derive_replay_name_appends_two_when_no_suffix() {
        let taken = HashSet::new();
        let name = derive_replay_name("auth-refactor", &taken).unwrap();
        assert_eq!(name, "auth-refactor-2");
    }

    #[test]
    fn derive_replay_name_increments_existing_suffix() {
        let taken = HashSet::new();
        let name = derive_replay_name("auth-refactor-2", &taken).unwrap();
        assert_eq!(name, "auth-refactor-3");
    }

    #[test]
    fn derive_replay_name_skips_taken_candidates() {
        let mut taken = HashSet::new();
        taken.insert("task-2".to_string());
        taken.insert("task-3".to_string());
        let name = derive_replay_name("task", &taken).unwrap();
        assert_eq!(name, "task-4");
    }

    #[test]
    fn derive_replay_name_counts_up_from_stripped_suffix_skipping_taken() {
        let mut taken = HashSet::new();
        taken.insert("job-5".to_string());
        taken.insert("job-6".to_string());
        // source is `job-4`, so we start at 5, skip 5 and 6, land on 7.
        let name = derive_replay_name("job-4", &taken).unwrap();
        assert_eq!(name, "job-7");
    }

    #[test]
    fn derive_replay_name_errors_when_over_30_chars() {
        // base is 29 chars, so base-2 is 31 — must refuse.
        let base = "a".repeat(29);
        let taken = HashSet::new();
        let result = derive_replay_name(&base, &taken);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("--as"), "should suggest --as override: {msg}");
        });
    }

    // ── cmd_replay ───────────────────────────────────────────────────────

    fn prompt_read_ok(body: &str) -> Result<String, SkulkError> {
        Ok(body.into())
    }

    #[test]
    fn cmd_replay_happy_path_auto_derives_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix the bug"),                 // read stored prompt
            Ok(mock_inventory_single_agent("skulk-task")), // inv for name derivation
            Ok("exists".into()),                           // base clone check
            Ok(mock_inventory_single_agent("skulk-task")), // inv inside create
            ssh_ok(),                                      // worktree
            ssh_ok(),                                      // tmux create
            ssh_ok(),                                      // persist prompt
            ssh_ok(),                                      // send prompt
        ]);
        let result = cmd_replay(&ssh, "task", None, false, None, None, &cfg, None);
        assert!(result.is_ok(), "replay should succeed: {result:?}");
        // Second-to-last call is the persist; last is send.
        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("~/.skulk/prompts/skulk-task-2.txt")),
            "new agent's prompt should be persisted under the derived name: {calls:?}"
        );
    }

    #[test]
    fn cmd_replay_passes_model_through() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix it"),
            Ok(mock_inventory_single_agent("skulk-task")),
            Ok("exists".into()),
            Ok(mock_inventory_single_agent("skulk-task")),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_replay(&ssh, "task", None, false, Some("sonnet"), None, &cfg, None,).is_ok());
        // The tmux-create call lives at index 5 (read, inv, base, inv, worktree, tmux).
        let tmux_call = &ssh.calls()[5];
        assert!(
            tmux_call.contains("--model sonnet"),
            "replay should thread --model through: {tmux_call}"
        );
    }

    #[test]
    fn cmd_replay_uses_explicit_new_name_when_provided() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix it"),
            Ok(mock_inventory_single_agent("skulk-task")),
            Ok("exists".into()),
            Ok(mock_inventory_single_agent("skulk-task")),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(
            cmd_replay(
                &ssh,
                "task",
                Some("retry-opus"),
                false,
                None,
                None,
                &cfg,
                None,
            )
            .is_ok()
        );
        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("~/.skulk/prompts/skulk-retry-opus.txt")),
            "prompt should be persisted under the explicit new name: {calls:?}"
        );
    }

    #[test]
    fn cmd_replay_rejects_invalid_source_name_before_ssh() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_replay(&ssh, "Bad Name", None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::Validation(_) => {});
        assert!(
            ssh.calls().is_empty(),
            "no SSH calls expected for invalid source name: {:?}",
            ssh.calls()
        );
    }

    #[test]
    fn cmd_replay_rejects_invalid_override_name_before_ssh() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_replay(
            &ssh,
            "task",
            Some("Bad Name"),
            false,
            None,
            None,
            &cfg,
            None,
        );
        assert_err!(result, SkulkError::Validation(_) => {});
        assert!(ssh.calls().is_empty(), "no SSH expected: {:?}", ssh.calls());
    }

    #[test]
    fn cmd_replay_missing_stored_prompt_returns_notfound() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("exit 1".into()))]);
        let result = cmd_replay(&ssh, "task", None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("task"));
        });
    }

    #[test]
    fn cmd_replay_override_name_conflict_errors_before_creation() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix it"),
            Ok(mock_inventory(
                &["skulk-task", "skulk-retry"],
                &[
                    ("skulk-task", "/path/skulk-task"),
                    ("skulk-retry", "/path/skulk-retry"),
                ],
                &["skulk-task", "skulk-retry"],
            )),
        ]);
        let result = cmd_replay(&ssh, "task", Some("retry"), false, None, None, &cfg, None);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("retry"));
            assert!(msg.contains("already exists"));
        });
    }

    #[test]
    fn cmd_replay_skips_taken_auto_names() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix it"),
            // Both `task-2` and `task-3` are taken, so derivation lands on task-4.
            Ok(mock_inventory(
                &["skulk-task", "skulk-task-2", "skulk-task-3"],
                &[
                    ("skulk-task", "/p/skulk-task"),
                    ("skulk-task-2", "/p/skulk-task-2"),
                    ("skulk-task-3", "/p/skulk-task-3"),
                ],
                &["skulk-task", "skulk-task-2", "skulk-task-3"],
            )),
            Ok("exists".into()),
            // Inside the create call, the second inventory probe also sees
            // the same state — `task-4` is still free.
            Ok(mock_inventory(
                &["skulk-task", "skulk-task-2", "skulk-task-3"],
                &[
                    ("skulk-task", "/p/skulk-task"),
                    ("skulk-task-2", "/p/skulk-task-2"),
                    ("skulk-task-3", "/p/skulk-task-3"),
                ],
                &["skulk-task", "skulk-task-2", "skulk-task-3"],
            )),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_replay(&ssh, "task", None, false, None, None, &cfg, None).is_ok());
        let calls = ssh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("~/.skulk/prompts/skulk-task-4.txt")),
            "should have derived task-4: {calls:?}"
        );
    }

    #[test]
    fn cmd_replay_inventory_failure_propagates() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix it"),
            Err(SkulkError::Diagnostic {
                message: "timed out".into(),
                suggestion: "retry".into(),
            }),
        ]);
        let result = cmd_replay(&ssh, "task", None, false, None, None, &cfg, None);
        assert_err!(result, SkulkError::Diagnostic { .. } => {});
    }

    #[test]
    fn cmd_replay_increments_numeric_suffix_on_source_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix it"),
            // task-2 is the source (which implies it exists on the remote).
            Ok(mock_inventory_single_agent("skulk-task-2")),
            Ok("exists".into()),
            Ok(mock_inventory_single_agent("skulk-task-2")),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_replay(&ssh, "task-2", None, false, None, None, &cfg, None).is_ok());
        let calls = ssh.calls();
        // Replaying `task-2` should land on `task-3`, not `task-2-2`.
        assert!(
            calls
                .iter()
                .any(|c| c.contains("~/.skulk/prompts/skulk-task-3.txt")),
            "replay of task-2 should land on task-3: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("skulk-task-2-2")),
            "should not produce double-suffixed name: {calls:?}"
        );
    }

    #[test]
    fn cmd_replay_passes_through_remote_control_and_claude_args() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            prompt_read_ok("fix it"),
            Ok(mock_empty_inventory()),
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(
            cmd_replay(
                &ssh,
                "task",
                None,
                true,
                None,
                Some("--verbose"),
                &cfg,
                None,
            )
            .is_ok()
        );
        let tmux_call = &ssh.calls()[5];
        assert!(
            tmux_call.contains("--remote-control"),
            "remote_control should flow through: {tmux_call}"
        );
        assert!(
            tmux_call.contains("--verbose"),
            "claude_args should flow through: {tmux_call}"
        );
    }
}
