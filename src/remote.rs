//! Remote operations on an agent's tmux session and git worktree.
//!
//! This module owns the shell text that runs on the remote host and the
//! partial-failure cleanup around it. Command handlers call the verbs
//! ([`start_session`], [`kill_session`], [`create_worktree`],
//! [`remove_worktree`], [`delete_branch`], ...) and see only
//! `Result<(), SkulkError>`. The [`Ssh`] trait is the seam: tests inject
//! `MockSsh` and assert on behavior, while the tests at the bottom of this
//! file pin the exact command text.

use crate::agent_ref::AgentRef;
use crate::config::{Config, DEFAULT_HARNESS, DEFAULT_INIT_SCRIPT};
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::shell_escape;

/// Seconds the remote waits before pasting an initial prompt, giving the
/// harness time to start.
pub(crate) const STARTUP_DELAY: u32 = 5;

/// Build the SSH command to kill a tmux session for an agent.
fn kill_session_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("tmux kill-session -t {}", agent.session_name())
}

/// Build the SSH command to remove an agent's git worktree.
fn remove_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let agent = AgentRef::new(name, cfg);
    format!(
        "cd {base_path} && git worktree remove --force {}",
        agent.worktree_path(cfg)
    )
}

/// Build the SSH command to delete an agent's git branch.
fn delete_branch_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let agent = AgentRef::new(name, cfg);
    format!("cd {base_path} && git branch -D {}", agent.branch_name())
}

/// Build the SSH command to delete an agent's Stop-hook state file.
///
/// The file lives at `~/.skulk/state/<session_name>` and is written by the
/// Claude Code `Stop` / `UserPromptSubmit` hooks (see `hooks_settings_json`).
/// `rm -f` makes a missing file a no-op so callers fire this unconditionally.
fn remove_state_file_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("rm -f ~/.skulk/state/{}", agent.session_name())
}

/// Build the SSH command used to confirm the agent's tmux session exists.
fn has_session_command(name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    format!("tmux has-session -t {}", agent.session_name())
}

/// Build a shell snippet that atomically writes `busy` to the idle marker.
///
/// Prepended to `tmux send-keys` chains in `cmd_send` and the initial-prompt
/// delivery so the marker is set before the prompt is delivered. This closes
/// the race where `skulk wait`, invoked immediately after `skulk send`, could
/// observe a stale `idle` (or `missing`) marker and return prematurely: the
/// agent's own `UserPromptSubmit` hook fires asynchronously after Claude Code
/// reads the terminal input, which can be milliseconds to seconds after
/// send-keys.
pub(crate) fn mark_busy_command(session_name: &str) -> String {
    format!("mkdir -p ~/.skulk/state && printf busy > ~/.skulk/state/{session_name}")
}

/// Harness launch flags that vary per `new` / `restart` / `upload` call.
#[derive(Clone, Copy, Default)]
pub(crate) struct LaunchOptions<'a> {
    pub remote_control: bool,
    pub model: Option<&'a str>,
    pub claude_args: Option<&'a str>,
}

/// Failure of [`start_session_or_kill`]: the original error plus whether the
/// follow-up kill of the partial session also failed.
pub(crate) struct SessionStartFailure {
    pub error: SkulkError,
    pub cleanup_failed: bool,
}

/// Create a git worktree on a fresh branch off the default branch and install
/// the harness hooks.
pub(crate) fn create_worktree(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    ssh.run(&create_worktree_command(name, cfg)).map(|_| ())
}

/// Create a git worktree on the agent's existing branch and install the
/// harness hooks (used by `skulk upload`).
pub(crate) fn create_worktree_on_existing_branch(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
) -> Result<(), SkulkError> {
    ssh.run(&create_worktree_hooks_command(name, cfg))
        .map(|_| ())
}

/// Start the agent's tmux session and launch the harness in it.
pub(crate) fn start_session(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
    opts: &LaunchOptions<'_>,
) -> Result<(), SkulkError> {
    ssh.run(&create_tmux_command(
        name,
        cfg,
        opts.remote_control,
        opts.model,
        opts.claude_args,
    ))
    .map(|_| ())
}

/// Start the agent's tmux session; on failure, kill any partially created
/// session.
///
/// Tmux creation is a two-step shell pipeline (`new-session && send-keys`).
/// If `new-session` succeeds but `send-keys` fails, an empty session would
/// remain and block a later start with "already running". The cleanup is
/// best-effort: the caller receives the original error and a flag telling it
/// whether the cleanup itself failed.
pub(crate) fn start_session_or_kill(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
    opts: &LaunchOptions<'_>,
) -> Result<(), SessionStartFailure> {
    start_session(ssh, name, cfg, opts).map_err(|error| SessionStartFailure {
        error,
        cleanup_failed: kill_session(ssh, name, cfg).is_err(),
    })
}

/// Kill the agent's tmux session.
pub(crate) fn kill_session(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    ssh.run(&kill_session_command(name, cfg)).map(|_| ())
}

/// Force-remove the agent's git worktree.
pub(crate) fn remove_worktree(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    ssh.run(&remove_worktree_command(name, cfg)).map(|_| ())
}

/// Delete the agent's git branch.
pub(crate) fn delete_branch(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    ssh.run(&delete_branch_command(name, cfg)).map(|_| ())
}

/// Remove the agent's Stop-hook state marker.
pub(crate) fn remove_state_file(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
) -> Result<(), SkulkError> {
    ssh.run(&remove_state_file_command(name, cfg)).map(|_| ())
}

/// Confirm the agent's tmux session exists.
pub(crate) fn ensure_session_exists(
    ssh: &impl Ssh,
    name: &str,
    cfg: &Config,
) -> Result<(), SkulkError> {
    ssh.run(&has_session_command(name, cfg)).map(|_| ())
}

/// Undo a worktree creation (remove worktree and delete branch).
///
/// Best-effort: when the rollback fails, a warning on stderr tells the user to
/// run `skulk destroy`. Callers keep reporting the failure that triggered the
/// rollback.
pub(crate) fn roll_back_worktree(ssh: &impl Ssh, name: &str, cfg: &Config) {
    if ssh.run(&rollback_worktree_command(name, cfg)).is_err() {
        eprintln!("{}", format_rollback_failure_warning(name));
    }
}

/// Paste the initial prompt into the agent's session after the startup delay.
pub(crate) fn send_initial_prompt(
    ssh: &impl Ssh,
    name: &str,
    prompt: &str,
    cfg: &Config,
) -> Result<(), SkulkError> {
    ssh.run(&send_prompt_command(name, prompt, cfg)).map(|_| ())
}

/// Store the prompt on the remote so `skulk replay` can re-send it.
pub(crate) fn persist_prompt(
    ssh: &impl Ssh,
    session_name: &str,
    prompt: &str,
) -> Result<(), SkulkError> {
    ssh.run(&persist_prompt_command(session_name, prompt))
        .map(|_| ())
}

/// Remove any stored prompt for the session.
pub(crate) fn clear_prompt(ssh: &impl Ssh, session_name: &str) -> Result<(), SkulkError> {
    ssh.run(&clear_prompt_command(session_name)).map(|_| ())
}

/// Build the in-tmux launch sequence: export SKULK_* vars, optionally source
/// `.env`, optionally run the init hook, then start the agent harness.
///
/// The harness binary is `cfg.harness` (default `"claude"`; set to `"opencode"`
/// for `OpenCode`). Per-harness flag handling:
///
/// - `--dangerously-skip-permissions`: Claude Code's TUI flag. `OpenCode` only
///   accepts it on `opencode run`, not on the TUI launch — for opencode we
///   instead write `opencode.json` with `"permission": "allow"` to the
///   worktree root (see [`create_worktree_command`]).
/// - `--remote-control`: Claude Code only. For other harnesses the flag is
///   skipped and a stderr warning is emitted if the user explicitly requested
///   it.
/// - `--model <name>`: shared.
///
/// The init hook is gated on file existence (`[ -f {init_script} ]`), so a
/// project without `.skulk/init.sh` starts the harness directly. When the hook
/// exists and exits non-zero, the harness does not start — the shell returns
/// to a prompt inside the tmux session so the user can `skulk connect` and
/// inspect.
///
/// `remote_control`, `model`, and `claude_args` are threaded onto the final
/// harness invocation the same way `create_tmux_command` used to build
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
    let harness = &cfg.harness;
    let is_claude = harness == DEFAULT_HARNESS;
    let permissions_flag = if is_claude {
        " --dangerously-skip-permissions"
    } else {
        ""
    };
    let remote_control_flag = if remote_control && is_claude {
        format!(" --remote-control {session}")
    } else {
        if remote_control && !is_claude {
            eprintln!(
                "warning: --remote-control is Claude Code only; ignoring for harness '{harness}'"
            );
        }
        String::new()
    };
    // model and claude_args are interpolated raw into the sequence. The whole
    // sequence is then `shell_escape`d once by the caller
    // (`create_tmux_command`) for the outer `send-keys '...'` wrapping —
    // pre-escaping here would double-escape single quotes.
    let model_flag = match model {
        Some(m) => format!(" --model {m}"),
        None => String::new(),
    };
    let extra_args = match claude_args {
        Some(args) if !args.is_empty() => format!(" {args}"),
        _ => String::new(),
    };
    let harness_cmd =
        format!("{harness}{permissions_flag}{remote_control_flag}{model_flag}{extra_args}");
    // Grouping `{ set -a; . ./.env; set +a; }` (not `&&`-chained) guarantees
    // `set +a` runs even if `.env` has a syntax error and sourcing fails —
    // otherwise the shell would stay in auto-export mode and leak locals
    // defined by init.sh or the harness into the environment.
    format!(
        "export SKULK_AGENT_NAME={name} SKULK_SESSION={session} SKULK_BRANCH={session} SKULK_WORKTREE={worktree}; \
         [ -f .env ] && {{ set -a; . ./.env; set +a; }}; \
         if [ -f {init_script} ]; then bash {init_script} && {harness_cmd}; else {harness_cmd}; fi"
    )
}

/// JSON for the worktree's `.claude/settings.local.json`.
///
/// Installs Claude Code `Stop` and `UserPromptSubmit` hooks that write an
/// "idle" / "busy" marker to `~/.skulk/state/<session_name>` on the remote.
/// `skulk wait` polls that file to detect when the agent finishes its turn.
///
/// The JSON uses only double quotes, so it can safely be wrapped in single
/// quotes when passed through a shell `printf` (see [`create_worktree_command`]).
fn hooks_settings_json(session_name: &str) -> String {
    format!(
        r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":"mkdir -p ~/.skulk/state && printf idle > ~/.skulk/state/{session_name}"}}]}}],"UserPromptSubmit":[{{"hooks":[{{"type":"command","command":"mkdir -p ~/.skulk/state && printf busy > ~/.skulk/state/{session_name}"}}]}}]}}}}"#
    )
}

/// TypeScript source for the worktree's `.opencode/plugins/skulk-state.ts`.
///
/// `OpenCode` plugins are loaded from `.opencode/plugins/` and can subscribe to
/// session events. This plugin handles the `session.idle` event — the
/// `OpenCode` equivalent of Claude Code's `Stop` hook — and writes the `idle`
/// marker to `~/.skulk/state/<session_name>` so `skulk wait` can detect when
/// the agent has finished its turn.
///
/// Known limitation: `OpenCode` has no clean equivalent of Claude Code's
/// `UserPromptSubmit` event (upstream issue #573), so this plugin does not
/// write the `busy` marker. `Skulk` itself writes `busy` from the send path
/// (see [`crate::commands::wait::mark_busy_command`]), but a `skulk wait`
/// invoked before any prompt has been sent will see the marker as missing
/// and return immediately as idle.
///
/// The source uses only double quotes and no single quotes so it can safely
/// be wrapped in single quotes for the outer shell `printf` invocation in
/// [`create_worktree_command`].
fn opencode_plugin_source(session_name: &str) -> String {
    // Note: this string is wrapped in single quotes by `printf '%s' '...'`,
    // so it must contain no `'` characters. Use double-quoted JS strings
    // throughout. Newlines inside the POSIX single-quoted argument are
    // literal — fine for this multi-line source.
    format!(
        "// Skulk session-state plugin for OpenCode.\n\
         // Writes \"idle\" to ~/.skulk/state/{session_name} on the session.idle event so\n\
         // `skulk wait` can detect when the agent has finished its turn.\n\
         //\n\
         // Limitation: OpenCode has no UserPromptSubmit equivalent yet\n\
         // (upstream issue #573), so this plugin does not write a \"busy\" marker.\n\
         // Skulk writes busy from the send path; wait may return early if invoked\n\
         // before any prompt has been sent.\n\
         import {{ mkdirSync, writeFileSync }} from \"node:fs\";\n\
         import {{ homedir }} from \"node:os\";\n\
         import {{ join }} from \"node:path\";\n\
         \n\
         export const SkulkStatePlugin = async () => {{\n\
         \x20 const stateDir = join(homedir(), \".skulk\", \"state\");\n\
         \x20 const stateFile = join(stateDir, \"{session_name}\");\n\
         \x20 return {{\n\
         \x20   event: async ({{ event }}: {{ event: {{ type: string }} }}) => {{\n\
         \x20     if (event.type === \"session.idle\") {{\n\
         \x20       mkdirSync(stateDir, {{ recursive: true }});\n\
         \x20       writeFileSync(stateFile, \"idle\");\n\
         \x20     }}\n\
         \x20   }},\n\
         \x20 }};\n\
         }};\n"
    )
}

/// Build the SSH command to create a git worktree for an agent and install
/// the harness-specific hook that powers `skulk wait`.
///
/// Bundles worktree creation with hook installation in a single SSH round-trip
/// so both succeed or fail together.
///
/// - **claude**: writes `<worktree>/.claude/settings.local.json` with `Stop`
///   and `UserPromptSubmit` hooks (see [`hooks_settings_json`]).
/// - **opencode**: writes `<worktree>/.opencode/plugins/skulk-state.ts`, a
///   TypeScript plugin that subscribes to the `session.idle` event and writes
///   the idle marker (see [`opencode_plugin_source`]). `OpenCode` has no
///   equivalent of Claude Code's `UserPromptSubmit`, so the busy marker is
///   only set by Skulk's own send path. `skulk wait` invoked before the agent
///   has processed a turn therefore returns immediately as idle — acceptable
///   for now per upstream `OpenCode` issue #573.
///   Also writes `<worktree>/opencode.json` with `{"permission":"allow"}` when
///   `cfg.auto_approve_permissions = true` — `OpenCode`'s TUI does not accept
///   `--dangerously-skip-permissions` (that flag only exists on `opencode run`),
///   so permission must be granted via project config. Skipped when the flag is
///   false so the user can manage permissions themselves.
///
/// For any other harness no hook is installed; `skulk wait` will treat the
/// missing marker file as idle and return immediately.
fn create_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let worktree_base = &cfg.worktree_base;
    let default_branch = &cfg.default_branch;
    let agent = AgentRef::new(name, cfg);
    let worktree = agent.worktree_path(cfg);
    let branch = agent.branch_name();
    // `-b {branch}` creates a fresh branch off the default branch for a brand-new agent.
    let base = format!(
        "mkdir -p {worktree_base} && cd {base_path} && \
         git worktree add -b {branch} {worktree} {default_branch}"
    );
    append_harness_hooks(base, name, cfg)
}

/// Build the SSH command to create a git worktree pointed at an **existing**
/// branch (no `-b`), then install the same harness hooks as
/// [`create_worktree_command`].
///
/// Used by `skulk upload`, which imports the local branch into the remote repo
/// via a git bundle *before* creating the worktree — so the branch already
/// exists and must not be re-created. The `worktree_base` mkdir and hook
/// installation are identical to the new-agent path; only the `git worktree
/// add` form differs.
fn create_worktree_hooks_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let worktree_base = &cfg.worktree_base;
    let agent = AgentRef::new(name, cfg);
    let worktree = agent.worktree_path(cfg);
    let branch = agent.branch_name();
    // No `-b`: the branch was already created by `git fetch <bundle>` in the upload flow.
    let base = format!(
        "mkdir -p {worktree_base} && cd {base_path} && \
         git worktree add {worktree} {branch}"
    );
    append_harness_hooks(base, name, cfg)
}

/// Append the harness-specific hook installation to a worktree-creation command.
///
/// Factored out of [`create_worktree_command`] so the new-branch path and
/// the existing-branch path used by `skulk upload`
/// ([`create_worktree_hooks_command`]) install identical hooks. See
/// [`create_worktree_command`] for the per-harness behavior.
fn append_harness_hooks(base: String, name: &str, cfg: &Config) -> String {
    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let worktree = agent.worktree_path(cfg);
    match cfg.harness.as_str() {
        DEFAULT_HARNESS => {
            let hooks_json = hooks_settings_json(&session_name);
            format!(
                "{base} && \
                 mkdir -p {worktree}/.claude && \
                 printf '%s' '{hooks_json}' > {worktree}/.claude/settings.local.json"
            )
        }
        "opencode" => {
            let plugin_src = opencode_plugin_source(&session_name);
            let plugin_cmd = format!(
                "{base} && \
                 mkdir -p {worktree}/.opencode/plugins && \
                 printf '%s' '{plugin_src}' > {worktree}/.opencode/plugins/skulk-state.ts"
            );
            if cfg.auto_approve_permissions {
                // OpenCode's TUI does not accept `--dangerously-skip-permissions`
                // (that flag only exists on `opencode run`), so we grant permission
                // via project config. `[ -e ]` preserves any existing opencode.json
                // the user has committed.
                let opencode_json = r#"{"permission":"allow"}"#;
                format!(
                    "{plugin_cmd} && \
                     [ -e {worktree}/opencode.json ] || printf '%s' '{opencode_json}' > {worktree}/opencode.json"
                )
            } else {
                plugin_cmd
            }
        }
        _ => base,
    }
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
fn create_tmux_command(
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

/// Build the SSH command to persist an agent's initial prompt to
/// `~/.skulk/prompts/<session_name>.txt` on the remote so `skulk replay`
/// can re-send the same task to a fresh agent.
///
/// Lives outside the worktree so it survives `skulk destroy` — a natural
/// replay flow is "that run went off the rails, tear it down, try a different
/// model on the same prompt".
///
/// Uses `printf '%s'` (not `echo`) so no trailing newline is added; the read
/// path can return bytes verbatim. The prompt is POSIX-escaped so the outer
/// SSH layer can transport it in a single-quoted shell argument.
fn persist_prompt_command(session_name: &str, prompt: &str) -> String {
    let escaped = shell_escape(prompt);
    format!(
        "mkdir -p ~/.skulk/prompts && \
         printf '%s' '{escaped}' > ~/.skulk/prompts/{session_name}.txt"
    )
}

/// Build the SSH command to clear any persisted prompt for a session.
///
/// Prompt files deliberately survive `skulk destroy` (see
/// [`persist_prompt_command`]). That means re-creating an agent with
/// the same name but no prompt would leave the previous incarnation's prompt
/// on disk, where `skulk replay <name>` would silently replay it against
/// the unrelated new agent. `cmd_new` calls this whenever it creates an
/// agent without a prompt so the stored file reflects the current
/// incarnation.
///
/// Uses `rm -f` so a missing file (the common case — first-time creation) is
/// not an error.
fn clear_prompt_command(session_name: &str) -> String {
    format!("rm -f ~/.skulk/prompts/{session_name}.txt")
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
fn send_prompt_command(name: &str, prompt: &str, cfg: &Config) -> String {
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
fn rollback_worktree_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let agent = AgentRef::new(name, cfg);
    let worktree = agent.worktree_path(cfg);
    let branch = agent.branch_name();
    format!("cd {base_path} && git worktree remove --force {worktree} && git branch -D {branch}")
}

/// Format the warning emitted when a post-tmux-failure rollback also fails.
///
/// Points the user at `skulk destroy <name>` (not `skulk gc`). A failed
/// rollback leaves both the worktree and its branch intact — `gc_find_orphans`
/// classifies that pair as an *archived* agent and refuses to reap it
/// (see `gc.rs`), so advising `skulk gc` strands the user. `skulk destroy`
/// re-fetches inventory and cleans worktree+branch together.
fn format_rollback_failure_warning(name: &str) -> String {
    format!(
        "Warning: failed to roll back worktree for agent '{name}'. Retry with `skulk destroy {name}`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, ssh_err, ssh_ok, test_config};

    #[test]
    fn create_worktree_command_generates_correct_shell() {
        let cfg = test_config();
        let cmd = create_worktree_command("my-task", &cfg);
        assert!(cmd.contains("mkdir -p ~/test-project-worktrees"));
        assert!(cmd.contains("git worktree add -b skulk-my-task"));
        assert!(cmd.contains("~/test-project-worktrees/skulk-my-task"));
        assert!(cmd.contains("main"));
    }

    #[test]
    fn create_worktree_hooks_command_uses_existing_branch_without_dash_b() {
        // Upload imports the branch via `git fetch <bundle>` first, so the
        // worktree must attach to the existing branch (no `-b`, no default-branch
        // argument).
        let cfg = test_config();
        let cmd = create_worktree_hooks_command("my-task", &cfg);
        assert!(cmd.contains("mkdir -p ~/test-project-worktrees"));
        assert!(
            cmd.contains("git worktree add ~/test-project-worktrees/skulk-my-task skulk-my-task"),
            "expected worktree add against existing branch, got: {cmd}"
        );
        assert!(
            !cmd.contains("git worktree add -b"),
            "must not create a new branch with -b, got: {cmd}"
        );
    }

    #[test]
    fn create_worktree_hooks_command_installs_hooks_settings_file() {
        let cfg = test_config();
        let cmd = create_worktree_hooks_command("my-task", &cfg);
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
    fn create_worktree_command_installs_hooks_settings_file() {
        let cfg = test_config();
        let cmd = create_worktree_command("my-task", &cfg);
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
    fn create_worktree_command_writes_opencode_plugin_for_opencode_harness() {
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        let cmd = create_worktree_command("my-task", &cfg);
        assert!(
            cmd.contains("~/test-project-worktrees/skulk-my-task/.opencode/plugins"),
            "expected .opencode/plugins directory creation: {cmd}"
        );
        assert!(
            cmd.contains("skulk-state.ts"),
            "expected plugin file write: {cmd}"
        );
        assert!(
            !cmd.contains("settings.local.json"),
            "claude-only hooks file must not appear for opencode harness: {cmd}"
        );
    }

    #[test]
    fn create_worktree_command_writes_opencode_json_when_auto_approve_enabled() {
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        cfg.auto_approve_permissions = true;
        let cmd = create_worktree_command("my-task", &cfg);
        assert!(
            cmd.contains("~/test-project-worktrees/skulk-my-task/opencode.json"),
            "expected opencode.json write at worktree root: {cmd}"
        );
        assert!(
            cmd.contains(r#""permission":"allow""#),
            "expected permission:allow JSON payload: {cmd}"
        );
    }

    #[test]
    fn create_worktree_command_preserves_existing_opencode_json() {
        // If the user has committed an opencode.json into the project, we must
        // not overwrite it. Guarded by `[ -e ... ] || ...`.
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        cfg.auto_approve_permissions = true;
        let cmd = create_worktree_command("my-task", &cfg);
        assert!(
            cmd.contains("[ -e ~/test-project-worktrees/skulk-my-task/opencode.json ] ||"),
            "expected guard against overwriting an existing opencode.json: {cmd}"
        );
    }

    #[test]
    fn create_worktree_command_skips_opencode_json_when_auto_approve_disabled() {
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        // auto_approve_permissions defaults to false
        let cmd = create_worktree_command("my-task", &cfg);
        assert!(
            !cmd.contains("opencode.json"),
            "opencode.json must not appear when auto_approve_permissions=false: {cmd}"
        );
        assert!(
            cmd.contains("skulk-state.ts"),
            "idle plugin must still be installed: {cmd}"
        );
    }

    #[test]
    fn create_worktree_command_skips_opencode_json_for_claude_harness() {
        let cfg = test_config();
        let cmd = create_worktree_command("my-task", &cfg);
        assert!(
            !cmd.contains("opencode.json"),
            "opencode.json must not appear for claude harness: {cmd}"
        );
    }

    #[test]
    fn create_worktree_command_skips_hook_install_for_unknown_harness() {
        // Unknown harness: bare worktree create only — `skulk wait` will
        // treat the absent marker as idle.
        let mut cfg = test_config();
        cfg.harness = "custom-harness".into();
        let cmd = create_worktree_command("my-task", &cfg);
        assert!(cmd.contains("git worktree add"));
        assert!(
            !cmd.contains(".claude") && !cmd.contains(".opencode"),
            "no harness-specific hook scaffolding should appear: {cmd}"
        );
    }

    #[test]
    fn opencode_plugin_source_subscribes_to_session_idle() {
        let src = opencode_plugin_source("skulk-my-task");
        assert!(
            src.contains("session.idle"),
            "plugin should subscribe to session.idle: {src}"
        );
        assert!(
            src.contains("~/.skulk/state/skulk-my-task") || src.contains("\"skulk-my-task\""),
            "plugin should reference per-session state path: {src}"
        );
    }

    #[test]
    fn opencode_plugin_source_writes_idle_marker() {
        let src = opencode_plugin_source("skulk-my-task");
        assert!(
            src.contains("\"idle\""),
            "plugin should write the literal string \"idle\": {src}"
        );
    }

    #[test]
    fn opencode_plugin_source_contains_no_single_quotes() {
        // The plugin source is wrapped in single quotes for shell printf, so
        // it must not contain any single quotes of its own.
        let src = opencode_plugin_source("skulk-my-task");
        assert!(!src.contains('\''), "source must not contain ': {src}");
    }

    #[test]
    fn opencode_plugin_source_documents_userpromptsubmit_limitation() {
        // The plugin doesn't write a busy marker because OpenCode has no
        // UserPromptSubmit equivalent yet. Future readers need to know.
        let src = opencode_plugin_source("skulk-my-task");
        assert!(
            src.to_lowercase().contains("userpromptsubmit") || src.to_lowercase().contains("busy"),
            "source should document the missing busy-marker limitation: {src}"
        );
    }

    #[test]
    fn opencode_plugin_source_is_syntactically_valid() {
        // Guard against a future `format!` template edit that produces broken
        // plugin source (unbalanced braces, stray quotes, etc.). The plugin
        // contains one TypeScript-only construct — a parameter type annotation
        // — so we strip it to JS and run `node --check`, which only parses
        // syntax (no module resolution, so `node:fs` specifiers are fine).
        // Skip when node isn't installed so the Rust suite stays toolchain-
        // light for contributors without a Node runtime.
        let src = opencode_plugin_source("skulk-my-task");
        let js = src.replace(": { event: { type: string } }", "");

        let node_available = std::process::Command::new("sh")
            .arg("-c")
            .arg("command -v node")
            .output()
            .is_ok_and(|o| o.status.success());
        if !node_available {
            eprintln!("skipping opencode plugin syntax check: node not installed");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "skulk_opencode_plugin_check_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("skulk-state.mjs");
        std::fs::write(&path, &js).expect("write plugin source");

        let output = std::process::Command::new("node")
            .args(["--check"])
            .arg(&path)
            .output()
            .expect("run node --check");

        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            output.status.success(),
            "node --check rejected the plugin source:\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
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
    fn create_tmux_command_with_remote_control_includes_flag() {
        let cfg = test_config();
        let cmd = create_tmux_command("my-task", &cfg, true, None, None);
        assert!(cmd.contains("tmux new-session -d -s skulk-my-task"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--remote-control skulk-my-task"));
    }

    #[test]
    fn create_tmux_command_without_remote_control_omits_flag() {
        let cfg = test_config();
        let cmd = create_tmux_command("my-task", &cfg, false, None, None);
        assert!(cmd.contains("tmux new-session -d -s skulk-my-task"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(
            !cmd.contains("--remote-control"),
            "flag should be absent when remote_control=false, got: {cmd}"
        );
    }

    #[test]
    fn create_tmux_command_with_model_includes_model_flag() {
        let cfg = test_config();
        let cmd = create_tmux_command("my-task", &cfg, false, Some("opus"), None);
        assert!(
            cmd.contains("--model opus"),
            "should include --model flag, got: {cmd}"
        );
    }

    #[test]
    fn create_tmux_command_without_model_omits_model_flag() {
        let cfg = test_config();
        let cmd = create_tmux_command("my-task", &cfg, false, None, None);
        assert!(
            !cmd.contains("--model"),
            "should omit --model flag when model is None, got: {cmd}"
        );
    }

    #[test]
    fn create_tmux_command_with_claude_args_appends_verbatim() {
        let cfg = test_config();
        let cmd = create_tmux_command("my-task", &cfg, false, None, Some("--allowed-tools Bash"));
        assert!(
            cmd.contains("--allowed-tools Bash"),
            "should append claude_args, got: {cmd}"
        );
    }

    #[test]
    fn create_tmux_command_with_empty_claude_args_appends_nothing() {
        let cfg = test_config();
        let cmd = create_tmux_command("my-task", &cfg, false, None, Some(""));
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
    fn create_tmux_command_claude_args_escapes_single_quotes() {
        let cfg = test_config();
        let cmd = create_tmux_command(
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
    fn create_tmux_command_claude_args_shell_metacharacters_pass_through_literally() {
        // Pins intentional design: the outer single-quote wrap keeps metacharacters
        // literal for the SSH + send-keys layer. The string is then typed into the
        // inner tmux shell which DOES re-evaluate them — callers pre-quote for that
        // shell themselves (documented on the flag and in README).
        let cfg = test_config();
        let cmd = create_tmux_command(
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
    fn create_tmux_command_model_and_claude_args_combine() {
        let cfg = test_config();
        let cmd = create_tmux_command("my-task", &cfg, true, Some("sonnet"), Some("--verbose"));
        assert!(cmd.contains("--remote-control skulk-my-task"));
        assert!(cmd.contains("--model sonnet"));
        assert!(cmd.contains("--verbose"));
    }

    #[test]
    fn send_prompt_command_simple() {
        let cfg = test_config();
        let cmd = send_prompt_command("my-task", "fix the bug", &cfg);
        assert!(cmd.contains("sleep 5"));
        assert!(cmd.contains("tmux set-buffer"));
        assert!(cmd.contains("tmux paste-buffer -p"));
        assert!(cmd.contains("'fix the bug'"));
        assert!(cmd.contains("Enter"));
    }

    #[test]
    fn send_prompt_command_marks_busy_before_paste() {
        let cfg = test_config();
        let cmd = send_prompt_command("my-task", "fix the bug", &cfg);
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
    fn send_prompt_command_splits_paste_and_submit() {
        let cfg = test_config();
        let cmd = send_prompt_command("my-task", "fix the bug", &cfg);
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
    fn send_prompt_command_with_quotes() {
        let cfg = test_config();
        let cmd = send_prompt_command("my-task", "it's broken", &cfg);
        assert!(cmd.contains("it'\\''s broken"));
    }

    #[test]
    fn send_prompt_command_handles_multiline() {
        let cfg = test_config();
        let prompt = "Line 1\nLine 2\nLine 3";
        let cmd = send_prompt_command("my-task", prompt, &cfg);
        // The full prompt lives inside a single tmux set-buffer argument, so
        // delivery is one bracketed-paste block — newlines do not submit.
        assert!(cmd.contains("'Line 1\nLine 2\nLine 3'"));
        assert!(cmd.contains("paste-buffer -p"));
    }

    #[test]
    fn send_prompt_command_deletes_buffer_atomically_with_paste() {
        // `paste-buffer -d` deletes the buffer as part of a successful paste so the
        // prompt content does not linger in tmux's server-wide buffer list. We assert
        // the `-d` flag is on the same `paste-buffer` invocation, not a separate
        // `delete-buffer` call (which could leak the buffer if paste failed first).
        let cfg = test_config();
        let cmd = send_prompt_command("my-task", "hi", &cfg);
        assert!(
            cmd.contains("paste-buffer -p -d"),
            "expected paste-buffer to use -d for atomic delete-on-paste, got: {cmd}"
        );
        assert!(
            !cmd.contains("delete-buffer"),
            "separate delete-buffer call should be gone in favor of paste-buffer -d"
        );
    }

    // ── persist_prompt_command ─────────────────────────────────────

    #[test]
    fn persist_prompt_command_writes_under_prompts_dir() {
        let cmd = persist_prompt_command("skulk-my-task", "fix the bug");
        assert!(
            cmd.contains("mkdir -p ~/.skulk/prompts"),
            "should create prompts dir first: {cmd}"
        );
        assert!(
            cmd.contains("~/.skulk/prompts/skulk-my-task.txt"),
            "target path should embed session name: {cmd}"
        );
    }

    #[test]
    fn persist_prompt_command_uses_printf_without_trailing_newline() {
        // `printf '%s'` is intentional: the read side returns bytes verbatim,
        // so any trailing newline introduced here would be fed back to Claude
        // on replay. `echo` would add one — regression guard.
        let cmd = persist_prompt_command("skulk-my-task", "hi");
        assert!(
            cmd.contains("printf '%s'"),
            "should use printf '%s' to avoid extra newline: {cmd}"
        );
        assert!(
            !cmd.contains("echo '"),
            "should not use echo (adds trailing newline): {cmd}"
        );
    }

    #[test]
    fn persist_prompt_command_escapes_single_quotes() {
        let cmd = persist_prompt_command("skulk-t", "it's broken");
        assert!(
            cmd.contains("it'\\''s broken"),
            "single quotes must be POSIX-escaped: {cmd}"
        );
    }

    #[test]
    fn persist_prompt_command_handles_multiline() {
        let cmd = persist_prompt_command("skulk-t", "line 1\nline 2");
        // Newlines inside a POSIX single-quoted string are literal, so no
        // special escaping is needed — just verify they survive as-is.
        assert!(
            cmd.contains("'line 1\nline 2'"),
            "multiline prompt must pass through verbatim: {cmd}"
        );
    }

    #[test]
    fn rollback_worktree_command_generates() {
        let cfg = test_config();
        let cmd = rollback_worktree_command("my-task", &cfg);
        assert!(cmd.contains("git worktree remove --force"));
        assert!(cmd.contains("~/test-project-worktrees/skulk-my-task"));
        assert!(cmd.contains("git branch -D skulk-my-task"));
    }

    // ── rollback-failure warning text ──────────────────────────────────
    //
    // A failed rollback leaves the worktree AND its branch intact. `skulk gc`
    // treats that pair as an *archived* agent and refuses to touch it, so
    // advising gc strands the user. These tests pin the corrected advice
    // (`skulk destroy <name>`) so the misdirection can't silently return.
    // Mirrors `format_partial_destroy_warning_points_at_destroy_not_gc`.

    #[test]
    fn format_rollback_failure_warning_points_at_destroy_not_gc() {
        let msg = format_rollback_failure_warning("my-task");
        assert!(msg.contains("my-task"), "should name the agent: {msg}");
        assert!(
            msg.contains("skulk destroy my-task"),
            "must tell user to retry with skulk destroy: {msg}"
        );
        assert!(
            !msg.contains("skulk gc"),
            "must NOT suggest skulk gc (it won't clean worktree+branch state): {msg}"
        );
    }

    #[test]
    fn format_rollback_failure_warning_mentions_rollback_context() {
        let msg = format_rollback_failure_warning("my-task");
        assert!(
            msg.to_lowercase().contains("roll back") || msg.to_lowercase().contains("rollback"),
            "warning should explain it's a rollback failure: {msg}"
        );
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

    #[test]
    fn build_launch_sequence_uses_opencode_when_harness_is_opencode() {
        // The configured harness is interpolated as the binary name in both
        // branches of the init-hook `if`.
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(
            seq.contains("opencode") && seq.contains("else opencode"),
            "expected opencode launch line in both branches: {seq}"
        );
        assert!(
            !seq.contains("claude"),
            "claude binary must not appear when harness is opencode: {seq}"
        );
    }

    #[test]
    fn build_launch_sequence_omits_dangerously_skip_permissions_for_opencode() {
        // OpenCode's TUI does not accept `--dangerously-skip-permissions`
        // (that flag only exists on `opencode run`). Permission is granted
        // via `opencode.json` instead — see create_worktree_command.
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        let seq = build_launch_sequence("my-task", &cfg, false, None, None);
        assert!(
            !seq.contains("--dangerously-skip-permissions"),
            "opencode launch must not include --dangerously-skip-permissions: {seq}"
        );
    }

    #[test]
    fn build_launch_sequence_skips_remote_control_for_non_claude_harness() {
        // --remote-control is a Claude Code-only flag; OpenCode rejects it.
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        let seq = build_launch_sequence("my-task", &cfg, true, None, None);
        assert!(
            !seq.contains("--remote-control"),
            "opencode launch must not include --remote-control even when requested: {seq}"
        );
    }

    #[test]
    fn build_launch_sequence_threads_remaining_flags_through_opencode() {
        // Regression guard: --model and claude_args must attach to the
        // configured harness binary. --remote-control and
        // --dangerously-skip-permissions are intentionally absent for
        // opencode.
        let mut cfg = test_config();
        cfg.harness = "opencode".into();
        let seq = build_launch_sequence(
            "my-task",
            &cfg,
            false,
            Some("anthropic/claude-opus-4-7"),
            Some("--verbose"),
        );
        assert!(
            seq.contains("opencode --model anthropic/claude-opus-4-7 --verbose"),
            "expected --model and claude_args chained onto opencode: {seq}"
        );
    }

    // ── verbs ──────────────────────────────────────────────────────────

    #[test]
    fn kill_session_runs_tmux_kill_session() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        let result = kill_session(&ssh, "my-task", &cfg);

        assert!(result.is_ok());
        assert_eq!(ssh.calls(), vec!["tmux kill-session -t skulk-my-task"]);
    }

    #[test]
    fn kill_session_propagates_ssh_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_err("no session")]);

        assert!(kill_session(&ssh, "my-task", &cfg).is_err());
    }

    #[test]
    fn remove_worktree_command_text_is_pinned() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        remove_worktree(&ssh, "my-task", &cfg).unwrap();

        assert_eq!(
            ssh.calls(),
            vec![
                "cd ~/test-project && git worktree remove --force ~/test-project-worktrees/skulk-my-task"
            ]
        );
    }

    #[test]
    fn delete_branch_command_text_is_pinned() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        delete_branch(&ssh, "my-task", &cfg).unwrap();

        assert_eq!(
            ssh.calls(),
            vec!["cd ~/test-project && git branch -D skulk-my-task"]
        );
    }

    #[test]
    fn remove_state_file_command_text_is_pinned() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        remove_state_file(&ssh, "my-task", &cfg).unwrap();

        assert_eq!(ssh.calls(), vec!["rm -f ~/.skulk/state/skulk-my-task"]);
    }

    #[test]
    fn ensure_session_exists_probes_with_has_session() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        ensure_session_exists(&ssh, "my-task", &cfg).unwrap();

        assert_eq!(ssh.calls(), vec!["tmux has-session -t skulk-my-task"]);
    }

    #[test]
    fn mark_busy_command_writes_busy_to_session_marker() {
        assert_eq!(
            mark_busy_command("skulk-my-task"),
            "mkdir -p ~/.skulk/state && printf busy > ~/.skulk/state/skulk-my-task"
        );
    }

    #[test]
    fn start_session_or_kill_succeeds_without_cleanup() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        let result = start_session_or_kill(&ssh, "my-task", &cfg, &LaunchOptions::default());

        assert!(result.is_ok());
        assert_eq!(ssh.calls().len(), 1);
    }

    #[test]
    fn start_session_or_kill_kills_partial_session_on_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_err("send-keys failed"), ssh_ok()]);

        let failure = start_session_or_kill(&ssh, "my-task", &cfg, &LaunchOptions::default())
            .err()
            .map(|f| (f.error.to_string(), f.cleanup_failed));

        let calls = ssh.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], "tmux kill-session -t skulk-my-task");
        let (message, cleanup_failed) = failure.unwrap_or_default();
        assert!(message.contains("send-keys failed"), "got: {message}");
        assert!(!cleanup_failed);
    }

    #[test]
    fn start_session_or_kill_reports_failed_cleanup() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_err("boom"), ssh_err("kill failed")]);

        let failure = start_session_or_kill(&ssh, "my-task", &cfg, &LaunchOptions::default())
            .err()
            .map(|f| (f.error.to_string(), f.cleanup_failed));

        let (message, cleanup_failed) = failure.unwrap_or_default();
        assert!(message.contains("boom"), "original error kept: {message}");
        assert!(cleanup_failed);
    }

    #[test]
    fn start_session_passes_launch_options_to_the_command() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);
        let opts = LaunchOptions {
            remote_control: true,
            model: Some("opus"),
            claude_args: Some("--verbose"),
        };

        start_session(&ssh, "my-task", &cfg, &opts).unwrap();

        let calls = ssh.calls();
        assert!(calls[0].contains("--remote-control"));
        assert!(calls[0].contains("--model opus"));
        assert!(calls[0].contains("--verbose"));
    }

    #[test]
    fn roll_back_worktree_runs_one_combined_command() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);

        roll_back_worktree(&ssh, "my-task", &cfg);

        assert_eq!(ssh.calls().len(), 1);
    }

    #[test]
    fn roll_back_worktree_swallows_ssh_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_err("rollback failed")]);

        roll_back_worktree(&ssh, "my-task", &cfg);

        assert_eq!(ssh.calls().len(), 1);
    }
}
