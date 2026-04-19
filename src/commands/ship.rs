use crate::agent_ref::AgentRef;
use crate::commands::interact::push_command;
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command that verifies `gh` and `claude` exist on the remote.
///
/// Both are hard requirements for `skulk ship`: `gh` opens the PR and `claude`
/// authors the description. Prepends `$HOME/.local/bin` to PATH so Claude
/// Code's default Linux install location is discoverable — non-login SSH does
/// not source `~/.profile` / `~/.bash_profile`, so user-level PATH additions
/// defined there are absent by default. Matches the PATH the agent's tmux
/// session (a login shell) already sees and `skulk doctor`'s probe.
///
/// Emits a space-separated list of missing tool names on stdout (empty if
/// both present) and always exits 0, so the caller can report exactly which
/// tool is missing instead of conflating the two.
pub(crate) fn precheck_command() -> String {
    "PATH=\"$HOME/.local/bin:$PATH\"; \
     missing=; \
     command -v gh > /dev/null 2>&1 || missing=\"${missing:+$missing }gh\"; \
     command -v claude > /dev/null 2>&1 || missing=\"${missing:+$missing }claude\"; \
     printf '%s' \"$missing\""
        .to_string()
}

/// Build the human-readable diagnostic for a precheck that reports missing tools.
///
/// `missing` is the space-separated list of tool names emitted by
/// `precheck_command`. Lists exactly which tool(s) are absent — no "and/or"
/// hedging — and suggests the install command for each.
pub(crate) fn missing_tools_diagnostic(missing: &str, host: &str) -> SkulkError {
    let tools: Vec<&str> = missing.split_whitespace().collect();
    let joined = match tools.as_slice() {
        [one] => (*one).to_string(),
        [a, b] => format!("{a} and {b}"),
        _ => tools.join(", "),
    };
    let mut suggestions: Vec<String> = Vec::with_capacity(tools.len());
    for tool in &tools {
        match *tool {
            "gh" => suggestions.push(format!(
                "Install GitHub CLI on {host} (https://cli.github.com) and ensure it is on PATH for non-login shells."
            )),
            "claude" => suggestions.push(format!(
                "Install Claude Code on {host} (https://docs.claude.com/en/docs/claude-code/setup). If it is already installed at ~/.local/bin/claude, ensure that directory is on PATH for non-login shells (e.g. add it in ~/.bashrc)."
            )),
            _ => {}
        }
    }
    SkulkError::Diagnostic {
        message: format!("{joined} not installed on {host} (or not on PATH for non-login SSH)."),
        suggestion: suggestions.join(" "),
    }
}

/// Prompt sent to `claude -p` to author the PR description from a piped diff.
///
/// Format contract is enforced by `ship_command`'s parsing: line 1 is the title,
/// line 2 is blank, line 3+ is the markdown body. Kept short and explicit so the
/// model doesn't wrap the output in code fences or commentary.
pub(crate) const DESCRIPTION_PROMPT: &str = "Read the git diff on stdin and write a GitHub pull request description. \
     The first line is the PR title (concise, imperative, no trailing period, max 70 chars). \
     Then a blank line. Then a Markdown body explaining what changed and why. \
     Output ONLY the title and body, no preamble, no commentary, no code fences.";

/// Build the SSH command that generates the PR description and opens the PR.
///
/// One round-trip: pipes `git diff` into `claude -p`, splits the resulting file
/// into title (line 1) and body (line 3+), validates both are present and the
/// title isn't a code fence, then invokes `gh pr create`. The temp directory
/// is removed on any exit (success, failure, abort) via an `EXIT` trap.
///
/// `set -o pipefail` ensures a `git diff` failure (e.g. unknown revision) is
/// surfaced rather than masked by `claude -p` exiting 0 on empty stdin.
/// `set -e` aborts immediately on any failed step. Both require bash, zsh,
/// ksh, or POSIX-2024 dash -- universal on developer servers.
pub(crate) fn ship_command(name: &str, cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let default_branch = &cfg.default_branch;
    let branch = AgentRef::new(name, cfg).branch_name();
    let prompt = DESCRIPTION_PROMPT;
    format!(
        "set -e; set -o pipefail; \
         PATH=\"$HOME/.local/bin:$PATH\"; \
         cd {base_path}; \
         T=$(mktemp -d); trap 'rm -rf \"$T\"' EXIT; \
         git diff {default_branch}...{branch} | claude -p '{prompt}' > \"$T/desc\"; \
         head -n 1 \"$T/desc\" > \"$T/title\"; \
         tail -n +3 \"$T/desc\" > \"$T/body\"; \
         [ -s \"$T/title\" ] || {{ echo 'claude returned no title' >&2; exit 1; }}; \
         [ -s \"$T/body\" ] || {{ echo 'claude returned no body' >&2; exit 1; }}; \
         [ \"$(head -c 3 \"$T/title\")\" != '```' ] || {{ echo 'claude returned a code-fenced title' >&2; exit 1; }}; \
         gh pr create \
           --base {default_branch} \
           --head {branch} \
           --title \"$(cat \"$T/title\")\" \
           --body-file \"$T/body\""
    )
}

/// Push an agent's branch and open a PR with a Claude-authored description.
///
/// Three SSH round-trips, in order:
///   1. Verify `gh` and `claude` are installed on the remote.
///   2. Push the branch with upstream tracking (reusing `push_command`).
///   3. Generate the description via `claude -p` and open the PR via `gh pr create`.
///
/// The PR URL printed by `gh pr create` is forwarded to stdout on success.
pub(crate) fn cmd_ship(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    let host = &cfg.host;
    let agent = AgentRef::new(name, cfg);

    let missing = ssh.run(&precheck_command())?;
    if !missing.is_empty() {
        return Err(missing_tools_diagnostic(&missing, host));
    }

    ssh.run(&push_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;

    let output = ssh
        .run(&ship_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, host))?;

    if !output.is_empty() {
        println!("{output}");
    }
    eprintln!("Opened PR for {}.", agent.branch_name());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, assert_err, ssh_ok, test_config};

    // ── precheck_command ──────────────────────────────────────────────────

    #[test]
    fn precheck_command_checks_gh_and_claude() {
        let cmd = precheck_command();
        assert!(cmd.contains("command -v gh"));
        assert!(cmd.contains("command -v claude"));
    }

    #[test]
    fn precheck_command_silences_stdout_and_stderr() {
        let cmd = precheck_command();
        assert!(cmd.contains("> /dev/null 2>&1"));
    }

    #[test]
    fn precheck_command_prepends_local_bin_to_path() {
        // ~/.local/bin is Claude Code's default install location on Linux.
        // Non-login SSH does not source ~/.profile, so without this prefix
        // `command -v claude` misses installations there.
        let cmd = precheck_command();
        assert!(
            cmd.contains("PATH=\"$HOME/.local/bin:$PATH\""),
            "precheck must prepend ~/.local/bin to PATH: {cmd}"
        );
    }

    #[test]
    fn precheck_command_emits_missing_tools_to_stdout() {
        let cmd = precheck_command();
        assert!(
            cmd.contains("printf '%s' \"$missing\""),
            "precheck must print the missing-tools list to stdout: {cmd}"
        );
    }

    // ── missing_tools_diagnostic ──────────────────────────────────────────

    #[test]
    fn missing_tools_diagnostic_names_single_tool() {
        let err = missing_tools_diagnostic("claude", "bluebubble");
        assert_err!(Err::<(), _>(err), SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("claude"));
            assert!(!message.contains("gh"));
            assert!(message.contains("bluebubble"));
            assert!(suggestion.contains("claude"));
        });
    }

    #[test]
    fn missing_tools_diagnostic_names_both_tools() {
        let err = missing_tools_diagnostic("gh claude", "bluebubble");
        assert_err!(Err::<(), _>(err), SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("gh"));
            assert!(message.contains("claude"));
            assert!(message.contains("and"));
            assert!(suggestion.contains("cli.github.com"));
            assert!(suggestion.contains("claude"));
        });
    }

    #[test]
    fn missing_tools_diagnostic_mentions_path_in_suggestion() {
        // Claude Code installs to ~/.local/bin by default on Linux; the fix
        // must nudge the user toward adding that to non-login PATH.
        let err = missing_tools_diagnostic("claude", "bluebubble");
        assert_err!(Err::<(), _>(err), SkulkError::Diagnostic { suggestion, .. } => {
            assert!(suggestion.contains(".local/bin") || suggestion.contains("PATH"));
        });
    }

    // ── ship_command ──────────────────────────────────────────────────────

    #[test]
    fn ship_command_runs_in_base_path() {
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(cmd.contains(&format!("cd {}", cfg.base_path)));
    }

    #[test]
    fn ship_command_prepends_local_bin_to_path() {
        // The `claude -p` invocation needs the same PATH fix as precheck:
        // without it, `claude` installed at ~/.local/bin is not found over
        // non-login SSH even though precheck reported it present.
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(
            cmd.contains("PATH=\"$HOME/.local/bin:$PATH\""),
            "ship_command must prepend ~/.local/bin to PATH: {cmd}"
        );
    }

    #[test]
    fn ship_command_sets_pipefail() {
        // Without pipefail, a failing `git diff` is masked by claude exiting 0
        // on empty stdin, producing a hallucinated PR description.
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(
            cmd.contains("set -o pipefail"),
            "ship_command must set pipefail: {cmd}"
        );
    }

    #[test]
    fn ship_command_uses_set_e_for_fail_fast() {
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(cmd.contains("set -e"), "ship_command must set -e: {cmd}");
    }

    #[test]
    fn ship_command_uses_session_prefix_and_default_branch() {
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(cmd.contains("main...skulk-feat"));
        assert!(cmd.contains("--base main"));
        assert!(cmd.contains("--head skulk-feat"));
    }

    #[test]
    fn ship_command_uses_configured_default_branch() {
        let mut cfg = test_config();
        cfg.default_branch = "develop".into();
        let cmd = ship_command("feat", &cfg);
        assert!(cmd.contains("develop...skulk-feat"));
        assert!(cmd.contains("--base develop"));
    }

    #[test]
    fn ship_command_pipes_diff_to_claude() {
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(cmd.contains("git diff main...skulk-feat | claude -p"));
    }

    #[test]
    fn ship_command_invokes_gh_pr_create_with_title_and_body_file() {
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(cmd.contains("gh pr create"));
        assert!(cmd.contains("--title"));
        assert!(cmd.contains("--body-file"));
    }

    #[test]
    fn ship_command_cleans_up_temp_dir_via_exit_trap() {
        // EXIT trap fires on success, failure, and `set -e` aborts -- so
        // cleanup always runs without needing to capture exit codes manually.
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(
            cmd.contains("trap 'rm -rf \"$T\"' EXIT"),
            "ship_command must register an EXIT trap to clean up: {cmd}"
        );
    }

    #[test]
    fn ship_command_aborts_on_empty_title() {
        // Guards against `claude -p` returning malformed output (one-line
        // response, empty file, etc.) that would otherwise produce a PR with
        // an empty title.
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(
            cmd.contains("[ -s \"$T/title\" ]"),
            "ship_command must validate title file is non-empty: {cmd}"
        );
    }

    #[test]
    fn ship_command_aborts_on_empty_body() {
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(
            cmd.contains("[ -s \"$T/body\" ]"),
            "ship_command must validate body file is non-empty: {cmd}"
        );
    }

    #[test]
    fn ship_command_rejects_code_fenced_title() {
        // Claude sometimes wraps output in ```markdown fences despite the
        // prompt's instructions; a fenced title would land verbatim in the PR.
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(
            cmd.contains("'```'"),
            "ship_command must reject titles starting with code fences: {cmd}"
        );
    }

    #[test]
    fn description_prompt_specifies_title_body_format() {
        assert!(DESCRIPTION_PROMPT.to_lowercase().contains("title"));
        assert!(DESCRIPTION_PROMPT.to_lowercase().contains("body"));
        assert!(DESCRIPTION_PROMPT.to_lowercase().contains("blank line"));
    }

    #[test]
    fn description_prompt_contains_no_single_quotes() {
        // The prompt is interpolated inside a single-quoted shell argument; an
        // apostrophe would terminate the quoting and break the command.
        assert!(
            !DESCRIPTION_PROMPT.contains('\''),
            "prompt must not contain single quotes"
        );
    }

    // ── cmd_ship ──────────────────────────────────────────────────────────

    #[test]
    fn cmd_ship_runs_precheck_push_then_ship_in_order() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()),                           // precheck: no missing tools
            ssh_ok(),                                    // push
            Ok("https://github.com/x/y/pull/42".into()), // gh pr create
        ]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let calls = ssh.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains("command -v gh"));
        assert!(calls[1].contains("git push -u origin skulk-feat"));
        assert!(calls[2].contains("gh pr create"));
    }

    #[test]
    fn cmd_ship_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_ship(&ssh, "Bad-Name", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn cmd_ship_diagnoses_missing_claude_only() {
        // Real-world case: gh lives in /usr/bin (found) but claude is at
        // ~/.local/bin (not on non-login PATH). Error must name only claude,
        // not "gh and/or claude".
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("claude".into())]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        assert_err!(result, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("claude"));
            assert!(!message.contains("gh "));
            assert!(!message.contains(" gh"));
            assert!(suggestion.contains("claude"));
        });
    }

    #[test]
    fn cmd_ship_diagnoses_missing_gh_only() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("gh".into())]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.contains("gh"));
            assert!(!message.contains("claude"));
        });
    }

    #[test]
    fn cmd_ship_diagnoses_both_missing() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("gh claude".into())]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        assert_err!(result, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("gh"));
            assert!(message.contains("claude"));
            assert!(suggestion.contains("cli.github.com"));
            assert!(suggestion.contains("claude"));
        });
    }

    #[test]
    fn cmd_ship_passes_through_precheck_ssh_errors() {
        // SSH-level failures (connection timeouts, auth rejection, etc.)
        // must surface as-is rather than be misreported as missing tools.
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection timed out.".into(),
            suggestion: "Check network.".into(),
        })]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.contains("timed out"));
        });
    }

    #[test]
    fn cmd_ship_classifies_push_branch_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()), // precheck OK (no missing tools)
            Err(SkulkError::SshFailed(
                "error: src refspec skulk-nope does not match any".into(),
            )),
        ]);
        let result = cmd_ship(&ssh, "nope", &cfg);
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("nope"));
        });
    }

    #[test]
    fn cmd_ship_propagates_pr_creation_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()), // precheck OK
            ssh_ok(),          // push OK
            Err(SkulkError::SshFailed(
                "a pull request for branch \"skulk-feat\" already exists".into(),
            )),
        ]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        assert_err!(result, SkulkError::SshFailed(msg) => {
            assert!(msg.contains("already exists"));
        });
    }

    #[test]
    fn cmd_ship_succeeds_with_empty_gh_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new()), ssh_ok(), ssh_ok()]);
        assert!(cmd_ship(&ssh, "feat", &cfg).is_ok());
    }
}
