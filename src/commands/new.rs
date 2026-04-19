use std::path::{Path, PathBuf};

use crate::agent_ref::AgentRef;
use crate::commands::prompt_source;
use crate::commands::wait::mark_busy_command;
use crate::config::{Config, DEFAULT_INIT_SCRIPT};
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::fetch_inventory;
use crate::ssh::Ssh;
use crate::util::{check_base_clone, shell_escape, validate_model, validate_name};

const STARTUP_DELAY: u32 = 5;

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

/// Build the in-tmux launch sequence: export SKULK_* vars, optionally source
/// `.env`, optionally run the init hook, then start Claude Code.
///
/// The init hook is gated on file existence (`[ -f {init_script} ]`), so a
/// project without `.skulk/init.sh` starts Claude directly. When the hook
/// exists and exits non-zero, Claude does not start — the shell returns to a
/// prompt inside the tmux session so the user can `skulk connect` and inspect.
///
/// `remote_control`, `model`, and `claude_args` are threaded onto the final
/// `claude` invocation the same way `agent_create_tmux_command` used to build
/// it before the init hook existed. See that function's docs for the caller's
/// quoting responsibilities on `claude_args`.
fn build_launch_sequence(
    name: &str,
    cfg: &Config,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
) -> String {
    let agent = AgentRef::new(name, cfg);
    let session = agent.session_name();
    let worktree = agent.worktree_path(cfg);
    let init_script = cfg.init_script.as_deref().unwrap_or(DEFAULT_INIT_SCRIPT);
    let remote_control_flag = if remote_control {
        format!(" --remote-control {session}")
    } else {
        String::new()
    };
    // model and claude_args are interpolated raw into the sequence. The whole
    // sequence is then `shell_escape`d once by the caller
    // (`agent_create_tmux_command`) for the outer `send-keys '...'` wrapping —
    // pre-escaping here would double-escape single quotes.
    let model_flag = match model {
        Some(m) => format!(" --model {m}"),
        None => String::new(),
    };
    let extra_args = match claude_args {
        Some(args) if !args.is_empty() => format!(" {args}"),
        _ => String::new(),
    };
    let claude_cmd = format!(
        "claude --dangerously-skip-permissions{remote_control_flag}{model_flag}{extra_args}"
    );
    // Grouping `{ set -a; . ./.env; set +a; }` (not `&&`-chained) guarantees
    // `set +a` runs even if `.env` has a syntax error and sourcing fails —
    // otherwise the shell would stay in auto-export mode and leak locals
    // defined by init.sh or claude into the environment.
    format!(
        "export SKULK_AGENT_NAME={name} SKULK_SESSION={session} SKULK_BRANCH={session} SKULK_WORKTREE={worktree}; \
         [ -f .env ] && {{ set -a; . ./.env; set +a; }}; \
         if [ -f {init_script} ]; then bash {init_script} && {claude_cmd}; else {claude_cmd}; fi"
    )
}

/// JSON for the worktree's `.claude/settings.local.json`.
///
/// Installs Claude Code `Stop` and `UserPromptSubmit` hooks that write an
/// "idle" / "busy" marker to `~/.skulk/state/<session_name>` on the remote.
/// `skulk wait` polls that file to detect when the agent finishes its turn.
///
/// The JSON uses only double quotes, so it can safely be wrapped in single
/// quotes when passed through a shell `printf` (see [`agent_create_worktree_command`]).
pub(crate) fn hooks_settings_json(session_name: &str) -> String {
    format!(
        r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":"mkdir -p ~/.skulk/state && printf idle > ~/.skulk/state/{session_name}"}}]}}],"UserPromptSubmit":[{{"hooks":[{{"type":"command","command":"mkdir -p ~/.skulk/state && printf busy > ~/.skulk/state/{session_name}"}}]}}]}}}}"#
    )
}

/// Build the SSH command to create a git worktree for an agent and install
/// Claude Code hooks powering `skulk wait`.
///
/// Bundles worktree creation with hook installation in a single SSH round-trip
/// so both succeed or fail together. The hook JSON is written to
/// `<worktree>/.claude/settings.local.json`, where Claude Code picks it up on
/// launch.
pub(crate) fn agent_create_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let worktree_base = &cfg.worktree_base;
    let default_branch = &cfg.default_branch;
    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let worktree = agent.worktree_path(cfg);
    let branch = agent.branch_name();
    let hooks_json = hooks_settings_json(&session_name);
    format!(
        "mkdir -p {worktree_base} && cd {base_path} && \
         git worktree add -b {branch} {worktree} {default_branch} && \
         mkdir -p {worktree}/.claude && \
         printf '%s' '{hooks_json}' > {worktree}/.claude/settings.local.json"
    )
}

/// Build the SSH command to create a tmux session and launch Claude Code for an agent.
///
/// Creates the session with a login shell (not a direct command) so the session
/// survives if Claude exits, then sends the launch sequence via send-keys.
/// Using a login shell also ensures `~/.local/bin` is in PATH.
///
/// The launch sequence (see `build_launch_sequence`) exports `SKULK_*` vars,
/// sources `<worktree>/.env` if present, runs the init hook if configured and
/// present, and finally starts Claude. If the init hook exits non-zero, Claude
/// does not start — the shell stays at a prompt so the user can investigate
/// with `skulk connect`.
///
/// When `remote_control` is true, Claude is launched with `--remote-control`
/// so the agent is reachable from the Claude Code mobile/web app. Off by default
/// because it triggers an upstream idle-death bug
/// (<https://github.com/anthropics/claude-code/issues/32982>); Skulk's own
/// commands work through tmux directly and do not need it.
///
/// When `model` is provided, Claude is launched with `--model <name>` so the
/// agent runs on a specific model (e.g. `opus`, `sonnet`, `claude-opus-4-7`).
/// The caller must pre-validate the value with `validate_model`; we only
/// escape for the outer single-quoted `send-keys` argument here.
///
/// When `claude_args` is provided, the raw string is appended to the Claude
/// command line. IMPORTANT: `tmux send-keys` *types* this string into the
/// remote shell, which then re-parses it — shell metacharacters (`$`,
/// `` ` ``, `;`, `(`, `)`, globs, unquoted whitespace) are re-evaluated by
/// that shell. Callers must pre-quote values that must reach Claude
/// literally (e.g. `--allowed-tools 'Bash(gh pr:*)'` — note the inner
/// single quotes). `shell_escape` here only protects the outer `send-keys`
/// quoting, not the inner shell.
pub(crate) fn agent_create_tmux_command(
    name: &str,
    cfg: &Config,
    remote_control: bool,
    model: Option<&str>,
    claude_args: Option<&str>,
) -> String {
    let agent = AgentRef::new(name, cfg);
    let session = agent.session_name();
    let worktree = agent.worktree_path(cfg);
    let sequence = build_launch_sequence(name, cfg, remote_control, model, claude_args);
    let escaped = shell_escape(&sequence);
    format!(
        "tmux new-session -d -s {session} -c {worktree} && \
         tmux send-keys -t {session} '{escaped}' C-m"
    )
}

/// Build the SSH command to send an initial prompt to an agent after a startup delay.
/// The sleep runs on the remote so it does not block the laptop CLI.
/// Checks that the session is still alive after sleeping before attempting delivery,
/// so it fails cleanly if Claude Code exited during startup.
///
/// Uses a tmux named buffer + `paste-buffer -p` (bracketed paste) so multi-line
/// prompts arrive as a single pasted block rather than line-by-line — each newline
/// in a `send-keys` call would otherwise submit a partial message to Claude Code.
/// After the paste we sleep briefly then send Enter separately, which defeats
/// Claude Code's paste-detection swallowing the trailing Enter as a newline.
///
/// `paste-buffer -d` deletes the buffer atomically with a successful paste, so a
/// prompt containing sensitive context never lingers in tmux's server-wide buffer
/// list. The buffer name is deterministic per agent (`skulk-prompt-<session>`),
/// so even in the rare race where the session dies between `has-session` and
/// `paste-buffer` and the buffer leaks, the next attempt for the same agent
/// overwrites it via `set-buffer`.
///
/// After the has-session check, writes `busy` to the idle marker (see
/// [`mark_busy_command`]) so a `skulk wait` invoked right after this command
/// returns sees `busy` instead of a stale `missing`/`idle` marker — the
/// agent's own `UserPromptSubmit` hook fires asynchronously and can lag
/// behind the paste.
pub(crate) fn agent_send_prompt_command(name: &str, prompt: &str, cfg: &Config) -> String {
    let escaped = shell_escape(prompt);
    let session_name = AgentRef::new(name, cfg).session_name();
    let buffer = format!("skulk-prompt-{session_name}");
    let mark_busy = mark_busy_command(&session_name);
    format!(
        "sleep {STARTUP_DELAY} && tmux has-session -t {session_name} && \
         {mark_busy} && \
         tmux set-buffer -b {buffer} -- '{escaped}' && \
         tmux paste-buffer -p -d -t {session_name} -b {buffer} && \
         sleep 0.1 && \
         tmux send-keys -t {session_name} Enter"
    )
}

/// Build the SSH command to roll back a worktree creation (remove worktree + delete branch).
pub(crate) fn agent_rollback_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let agent = AgentRef::new(name, cfg);
    let worktree = agent.worktree_path(cfg);
    let branch = agent.branch_name();
    format!("cd {base_path} && git worktree remove --force {worktree} && git branch -D {branch}")
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
/// 8. Send prompt if provided
///    - On failure: warn user, keep agent alive
/// 9. Print success output
//
// Explicitly allow `too_many_arguments` / `too_many_lines`: `cmd_new` is the
// top-level orchestrator for the `new` command, and each argument is a distinct
// externally-supplied input required by one of the steps above. Grouping
// them into a struct or splitting the function would hide the linear sequence
// that makes this function easy to follow.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    let base_path = &cfg.base_path;
    let host = &cfg.host;
    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let branch = agent.branch_name();
    let worktree = agent.worktree_path(cfg);

    // Step 1: Resolve initial prompt (if any). clap `conflicts_with` on the CLI
    // enforces that both --github and --from can't be set; the unreachable arm
    // is a defensive guard for non-CLI callers.
    let prompt: Option<String> = match (github, from) {
        (None, None) => None,
        (Some(id), None) => Some(prompt_source::load_github_prompt(ssh, id, &branch, cfg)?),
        (None, Some(path)) => Some(prompt_source::load_file_prompt(path, &branch)?),
        (Some(_), Some(_)) => unreachable!("--github and --from are mutually exclusive"),
    };

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
    let has_session = inv.sessions.contains(&session_name);
    let has_worktree = inv.worktrees.contains_key(&session_name);
    if has_session {
        return Err(SkulkError::Validation(format!(
            "Agent '{name}' already exists."
        )));
    }
    if has_worktree {
        return Err(SkulkError::Validation(format!(
            "Agent '{name}' already has a worktree (archived or crashed).\n  \
             Resume it: `skulk restart {name}`\n  \
             Or wipe it: `skulk destroy {name}`"
        )));
    }

    // Step 3: Create worktree
    ssh.run(&agent_create_worktree_command(name, cfg))?;

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
    if let Err(e) = ssh.run(&agent_create_tmux_command(
        name,
        cfg,
        remote_control,
        model,
        claude_args,
    )) {
        // Attempt rollback (best-effort)
        if ssh
            .run(&agent_rollback_worktree_command(name, cfg))
            .is_err()
        {
            eprintln!(
                "Warning: Failed to clean up worktree for agent '{name}'. Run `skulk gc` to clean up."
            );
        }
        return Err(e);
    }

    // Step 6: Send prompt if provided
    let prompt_status = if let Some(prompt_text) = prompt.as_deref() {
        if ssh
            .run(&agent_send_prompt_command(name, prompt_text, cfg))
            .is_ok()
        {
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

    let permissions_line = if remote_control {
        "  Mode: remote-control (skip-permissions)"
    } else {
        "  Mode: skip-permissions"
    };

    let model_line = model.map(|m| format!("\n  Model: {m}")).unwrap_or_default();
    let extra_args_line = claude_args
        .filter(|s| !s.is_empty())
        .map(|a| format!("\n  Extra Claude args: {a}"))
        .unwrap_or_default();

    println!(
        "Agent '{name}' created.\n\
         \x20 Branch: {branch}\n\
         \x20 Worktree: {worktree}\n\
         {permissions_line}{model_line}{extra_args_line}\n\
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
    fn agent_create_worktree_command_generates_correct_shell() {
        let cfg = test_config();
        let cmd = agent_create_worktree_command("my-task", &cfg);
        assert!(cmd.contains("mkdir -p ~/test-project-worktrees"));
        assert!(cmd.contains("git worktree add -b skulk-my-task"));
        assert!(cmd.contains("~/test-project-worktrees/skulk-my-task"));
        assert!(cmd.contains("main"));
    }

    #[test]
    fn agent_create_worktree_command_installs_hooks_settings_file() {
        let cfg = test_config();
        let cmd = agent_create_worktree_command("my-task", &cfg);
        assert!(
            cmd.contains("~/test-project-worktrees/skulk-my-task/.claude"),
            "expected .claude directory creation: {cmd}"
        );
        assert!(
            cmd.contains("settings.local.json"),
            "expected hooks settings file write: {cmd}"
        );
    }

    #[test]
    fn hooks_settings_json_includes_stop_hook_for_session() {
        let json = hooks_settings_json("skulk-my-task");
        assert!(json.contains("\"Stop\""), "missing Stop hook: {json}");
        assert!(
            json.contains("printf idle > ~/.skulk/state/skulk-my-task"),
            "Stop command should write idle marker: {json}"
        );
    }

    #[test]
    fn hooks_settings_json_includes_user_prompt_submit_hook_for_session() {
        let json = hooks_settings_json("skulk-my-task");
        assert!(
            json.contains("\"UserPromptSubmit\""),
            "missing UserPromptSubmit hook: {json}"
        );
        assert!(
            json.contains("printf busy > ~/.skulk/state/skulk-my-task"),
            "UserPromptSubmit command should write busy marker: {json}"
        );
    }

    #[test]
    fn hooks_settings_json_contains_no_single_quotes() {
        // The JSON is wrapped in single quotes for shell echo, so it must not
        // contain any single quotes of its own.
        let json = hooks_settings_json("skulk-my-task");
        assert!(!json.contains('\''), "json must not contain ': {json}");
    }

    #[test]
    fn agent_create_tmux_command_with_remote_control_includes_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, true, None, None);
        assert!(cmd.contains("tmux new-session -d -s skulk-my-task"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--remote-control skulk-my-task"));
    }

    #[test]
    fn agent_create_tmux_command_without_remote_control_omits_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, None, None);
        assert!(cmd.contains("tmux new-session -d -s skulk-my-task"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(
            !cmd.contains("--remote-control"),
            "flag should be absent when remote_control=false, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_with_model_includes_model_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, Some("opus"), None);
        assert!(
            cmd.contains("--model opus"),
            "should include --model flag, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_without_model_omits_model_flag() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, None, None);
        assert!(
            !cmd.contains("--model"),
            "should omit --model flag when model is None, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_with_claude_args_appends_verbatim() {
        let cfg = test_config();
        let cmd =
            agent_create_tmux_command("my-task", &cfg, false, None, Some("--allowed-tools Bash"));
        assert!(
            cmd.contains("--allowed-tools Bash"),
            "should append claude_args, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_with_empty_claude_args_appends_nothing() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command("my-task", &cfg, false, None, Some(""));
        // Empty claude_args must not introduce a trailing space after the claude
        // command. Both branches of the init-hook `if` emit the bare claude
        // invocation followed by `;` (then-branch) or `; fi` (else-branch).
        assert!(
            cmd.contains("claude --dangerously-skip-permissions;"),
            "empty claude_args should not introduce trailing space, got: {cmd}"
        );
        assert!(
            !cmd.contains("claude --dangerously-skip-permissions ;"),
            "empty claude_args produced a stray space before `;`: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_claude_args_escapes_single_quotes() {
        let cfg = test_config();
        let cmd = agent_create_tmux_command(
            "my-task",
            &cfg,
            false,
            None,
            Some("--system-prompt 'be nice'"),
        );
        // Single quotes in claude_args must be escaped for the surrounding single-quoted send-keys string.
        assert!(
            cmd.contains("--system-prompt '\\''be nice'\\''"),
            "single quotes in claude_args should be POSIX-escaped, got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_claude_args_shell_metacharacters_pass_through_literally() {
        // Pins intentional design: the outer single-quote wrap keeps metacharacters
        // literal for the SSH + send-keys layer. The string is then typed into the
        // inner tmux shell which DOES re-evaluate them — callers pre-quote for that
        // shell themselves (documented on the flag and in README).
        let cfg = test_config();
        let cmd = agent_create_tmux_command(
            "my-task",
            &cfg,
            false,
            None,
            Some("--flag $(whoami) `id` ; true"),
        );
        assert!(
            cmd.contains("$(whoami)") && cmd.contains("`id`") && cmd.contains("; true"),
            "metacharacters must survive the outer layer verbatim; got: {cmd}"
        );
    }

    #[test]
    fn agent_create_tmux_command_model_and_claude_args_combine() {
        let cfg = test_config();
        let cmd =
            agent_create_tmux_command("my-task", &cfg, true, Some("sonnet"), Some("--verbose"));
        assert!(cmd.contains("--remote-control skulk-my-task"));
        assert!(cmd.contains("--model sonnet"));
        assert!(cmd.contains("--verbose"));
    }

    #[test]
    fn agent_send_prompt_command_simple() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "fix the bug", &cfg);
        assert!(cmd.contains("sleep 5"));
        assert!(cmd.contains("tmux set-buffer"));
        assert!(cmd.contains("tmux paste-buffer -p"));
        assert!(cmd.contains("'fix the bug'"));
        assert!(cmd.contains("Enter"));
    }

    #[test]
    fn agent_send_prompt_command_marks_busy_before_paste() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "fix the bug", &cfg);
        let busy_idx = cmd.find("printf busy").expect("busy marker write missing");
        let paste_idx = cmd
            .find("tmux paste-buffer")
            .expect("paste-buffer step missing");
        assert!(
            busy_idx < paste_idx,
            "busy marker must be written before paste: {cmd}"
        );
    }

    #[test]
    fn agent_send_prompt_command_splits_paste_and_submit() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "fix the bug", &cfg);
        let paste_idx = cmd.find("tmux paste-buffer").expect("paste step missing");
        let submit_sleep_idx = cmd[paste_idx..]
            .find("sleep 0.1")
            .map(|i| i + paste_idx)
            .expect("submit delay missing");
        let enter_idx = cmd[submit_sleep_idx..]
            .find("send-keys")
            .map(|i| i + submit_sleep_idx)
            .expect("submit send-keys missing");
        assert!(paste_idx < submit_sleep_idx);
        assert!(submit_sleep_idx < enter_idx);
        assert!(cmd[enter_idx..].contains("Enter"));
    }

    #[test]
    fn agent_send_prompt_command_with_quotes() {
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "it's broken", &cfg);
        assert!(cmd.contains("it'\\''s broken"));
    }

    #[test]
    fn agent_send_prompt_command_handles_multiline() {
        let cfg = test_config();
        let prompt = "Line 1\nLine 2\nLine 3";
        let cmd = agent_send_prompt_command("my-task", prompt, &cfg);
        // The full prompt lives inside a single tmux set-buffer argument, so
        // delivery is one bracketed-paste block — newlines do not submit.
        assert!(cmd.contains("'Line 1\nLine 2\nLine 3'"));
        assert!(cmd.contains("paste-buffer -p"));
    }

    #[test]
    fn agent_send_prompt_command_deletes_buffer_atomically_with_paste() {
        // `paste-buffer -d` deletes the buffer as part of a successful paste so the
        // prompt content does not linger in tmux's server-wide buffer list. We assert
        // the `-d` flag is on the same `paste-buffer` invocation, not a separate
        // `delete-buffer` call (which could leak the buffer if paste failed first).
        let cfg = test_config();
        let cmd = agent_send_prompt_command("my-task", "hi", &cfg);
        assert!(
            cmd.contains("paste-buffer -p -d"),
            "expected paste-buffer to use -d for atomic delete-on-paste, got: {cmd}"
        );
        assert!(
            !cmd.contains("delete-buffer"),
            "separate delete-buffer call should be gone in favor of paste-buffer -d"
        );
    }

    #[test]
    fn agent_rollback_worktree_command_generates() {
        let cfg = test_config();
        let cmd = agent_rollback_worktree_command("my-task", &cfg);
        assert!(cmd.contains("git worktree remove --force"));
        assert!(cmd.contains("~/test-project-worktrees/skulk-my-task"));
        assert!(cmd.contains("git branch -D skulk-my-task"));
    }

    #[test]
    fn cmd_new_succeeds_without_prompt() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            ssh_ok(),
        ]);
        assert!(cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None).is_ok());
    }

    #[test]
    fn cmd_new_succeeds_with_prompt() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
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
            ssh_ok(),
            ssh_ok(),
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
            ssh_ok(),
            ssh_ok(),
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
            ssh_ok(),
            ssh_ok(),
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
            ssh_ok(),
            ssh_ok(),
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
            ssh_ok(),
        ]);
        let result = cmd_new(&ssh, "test", None, None, false, None, None, &cfg, None);
        assert!(result.is_err());
    }

    #[test]
    fn cmd_new_tmux_fails_rollback_also_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            Err(SkulkError::SshFailed("tmux creation failed".into())),
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
            ssh_ok(),
            ssh_ok(),
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

    // ── build_launch_sequence ──────────────────────────────────────────

    #[test]
    fn build_launch_sequence_exports_skulk_env_vars() {
        let cfg = test_config();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(seq.contains("SKULK_AGENT_NAME=my-task"));
        assert!(seq.contains("SKULK_SESSION=skulk-my-task"));
        assert!(seq.contains("SKULK_BRANCH=skulk-my-task"));
        assert!(seq.contains("SKULK_WORKTREE=~/test-project-worktrees/skulk-my-task"));
    }

    #[test]
    fn build_launch_sequence_sources_dotenv_conditionally() {
        let cfg = test_config();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(
            seq.contains("[ -f .env ]"),
            "expected conditional .env check: {seq}"
        );
        assert!(seq.contains(". ./.env"), "expected dotenv source: {seq}");
    }

    #[test]
    fn build_launch_sequence_dotenv_uses_grouping_to_guarantee_set_plus_a() {
        // Regression: the old `set -a && . ./.env && set +a` chain would skip
        // `set +a` if sourcing `.env` failed (syntax error in user's file),
        // leaving the shell in auto-export mode. Grouping with `{ ...; }`
        // ensures `set +a` runs regardless of `.env` sourcing outcome.
        let cfg = test_config();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(
            seq.contains("{ set -a; . ./.env; set +a; }"),
            "expected grouped dotenv source so set +a always runs: {seq}"
        );
        assert!(
            !seq.contains("set -a && . ./.env && set +a"),
            "old &&-chain would skip set +a on source failure: {seq}"
        );
    }

    #[test]
    fn build_launch_sequence_defaults_init_script_to_skulk_init_sh() {
        let cfg = test_config();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(
            seq.contains("[ -f .skulk/init.sh ]"),
            "expected default init script path: {seq}"
        );
        assert!(seq.contains("bash .skulk/init.sh"));
    }

    #[test]
    fn build_launch_sequence_respects_configured_override() {
        let mut cfg = test_config();
        cfg.init_script = Some("scripts/setup-agent.sh".into());
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(
            seq.contains("[ -f scripts/setup-agent.sh ]"),
            "expected override init script path: {seq}"
        );
        assert!(seq.contains("bash scripts/setup-agent.sh"));
        assert!(
            !seq.contains(".skulk/init.sh"),
            "default path should not appear when override set: {seq}"
        );
    }

    #[test]
    fn build_launch_sequence_init_hook_chained_with_claude_via_and() {
        // Hard-fail guarantee: when init.sh exists and fails, claude must not start.
        // We express that as `bash init.sh && claude`.
        let cfg = test_config();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        let hook_idx = seq
            .find("bash .skulk/init.sh")
            .expect("init hook invocation missing");
        let and_idx = seq[hook_idx..]
            .find("&&")
            .map(|i| i + hook_idx)
            .expect("hard-fail && missing after init.sh");
        let claude_idx = seq[and_idx..]
            .find("claude --dangerously-skip-permissions")
            .map(|i| i + and_idx)
            .expect("claude launch missing after &&");
        assert!(hook_idx < and_idx);
        assert!(and_idx < claude_idx);
    }

    #[test]
    fn build_launch_sequence_runs_claude_when_init_hook_absent() {
        // If the init file doesn't exist at runtime, claude still launches (else branch).
        let cfg = test_config();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(
            seq.contains("else claude --dangerously-skip-permissions"),
            "expected unconditional claude launch in else branch: {seq}"
        );
    }

    // ── cmd_new .env upload behavior ───────────────────────────────────

    #[test]
    fn cmd_new_uploads_local_env_when_provided() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(), // worktree
            ssh_ok(), // tmux create + send-keys
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
            ssh_ok(),
            ssh_ok(),
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
}
