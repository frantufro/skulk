use crate::agent_ref::AgentRef;
use crate::commands::interact::push_command;
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::ssh::Ssh;
use crate::util::validate_name;

/// Build the SSH command that verifies `gh` and `claude` exist on the remote.
///
/// Both are hard requirements for `skulk ship`: `gh` opens the PR and `claude`
/// authors the description. Returning a non-zero exit lets the caller diagnose
/// either as missing without parsing stderr.
pub(crate) fn precheck_command() -> String {
    "command -v gh > /dev/null 2>&1 && command -v claude > /dev/null 2>&1".to_string()
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

    ssh.run(&precheck_command()).map_err(|e| match e {
        SkulkError::SshFailed(_) => SkulkError::Diagnostic {
            message: format!("`gh` and/or `claude` not installed on {host}."),
            suggestion: format!(
                "Install both on {host}: GitHub CLI (https://cli.github.com) and Claude Code (https://docs.claude.com/en/docs/claude-code/setup)."
            ),
        },
        other => other,
    })?;

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
    use crate::testutil::{MockSsh, test_config};

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

    // ── ship_command ──────────────────────────────────────────────────────

    #[test]
    fn ship_command_runs_in_base_path() {
        let cfg = test_config();
        let cmd = ship_command("feat", &cfg);
        assert!(cmd.contains(&format!("cd {}", cfg.base_path)));
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
            Ok(String::new()),                           // precheck
            Ok(String::new()),                           // push
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
    fn cmd_ship_diagnoses_missing_gh_or_claude() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("exit code 127".into()))]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        match result {
            Err(SkulkError::Diagnostic {
                message,
                suggestion,
            }) => {
                assert!(message.contains("gh"));
                assert!(message.contains("claude"));
                assert!(suggestion.contains("cli.github.com"));
                assert!(suggestion.contains("claude"));
            }
            other => panic!("expected Diagnostic, got {other:?}"),
        }
    }

    #[test]
    fn cmd_ship_passes_through_non_ssh_failed_precheck_errors() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection timed out.".into(),
            suggestion: "Check network.".into(),
        })]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        match result {
            Err(SkulkError::Diagnostic { message, .. }) => {
                assert!(message.contains("timed out"));
            }
            other => panic!("expected timeout Diagnostic, got {other:?}"),
        }
    }

    #[test]
    fn cmd_ship_classifies_push_branch_not_found() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()), // precheck OK
            Err(SkulkError::SshFailed(
                "error: src refspec skulk-nope does not match any".into(),
            )),
        ]);
        let result = cmd_ship(&ssh, "nope", &cfg);
        match result {
            Err(SkulkError::NotFound(msg)) => assert!(msg.contains("nope")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn cmd_ship_propagates_pr_creation_failure() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()), // precheck OK
            Ok(String::new()), // push OK
            Err(SkulkError::SshFailed(
                "a pull request for branch \"skulk-feat\" already exists".into(),
            )),
        ]);
        let result = cmd_ship(&ssh, "feat", &cfg);
        match result {
            Err(SkulkError::SshFailed(msg)) => {
                assert!(msg.contains("already exists"));
            }
            other => panic!("expected SshFailed, got {other:?}"),
        }
    }

    #[test]
    fn cmd_ship_succeeds_with_empty_gh_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_ship(&ssh, "feat", &cfg).is_ok());
    }
}
