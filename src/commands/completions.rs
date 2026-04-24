//! `skulk completions <shell>` — generate shell completion scripts.
//!
//! Static completion (subcommands, flags) comes from `clap_complete`. A
//! hand-rolled per-shell snippet is appended that calls `skulk list` to
//! tab-complete agent names for commands that take one as an argument.
//!
//! The dynamic snippet runs `skulk --no-color list` at completion time and
//! filters the first column with awk, keeping only words that look like valid
//! agent names (`[a-z0-9][a-z0-9-]*`). That drops the header row and the
//! "No agents running." empty-state line without needing any structured
//! output format from the binary.

use std::io;

use clap::{CommandFactory, ValueEnum};
use clap_complete::{Shell, generate};

use crate::Cli;
use crate::error::SkulkError;

/// Binary name used when generating completion scripts.
const BIN_NAME: &str = "skulk";

/// Subcommands whose first positional argument is an existing agent name.
///
/// Kept in sync with `Commands` in `main.rs`. Commands that take a *new*
/// agent name (e.g. `new`) or no agent at all (e.g. `list`, `gc`) are
/// deliberately excluded so we don't propose stale names for `new` or
/// spam irrelevant names elsewhere.
const AGENT_COMMANDS: &[&str] = &[
    "archive",
    "connect",
    "destroy",
    "diff",
    "disconnect",
    "git-log",
    "logs",
    "push",
    "restart",
    "send",
    "ship",
    "status",
    "transcript",
    "wait",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

impl From<CompletionShell> for Shell {
    fn from(s: CompletionShell) -> Self {
        match s {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Fish => Shell::Fish,
        }
    }
}

/// Produce the full completion script for `shell` as a string.
///
/// Separated from `cmd_completions` so tests can assert on the output without
/// capturing stdout.
pub(crate) fn render(shell: CompletionShell) -> String {
    let mut cmd = Cli::command();
    let mut buf: Vec<u8> = Vec::new();
    generate(Shell::from(shell), &mut cmd, BIN_NAME, &mut buf);
    buf.extend_from_slice(dynamic_snippet(shell).as_bytes());
    String::from_utf8_lossy(&buf).into_owned()
}

/// Write the completion script for `shell` to stdout.
pub(crate) fn cmd_completions(shell: CompletionShell) -> Result<(), SkulkError> {
    let script = render(shell);
    io::Write::write_all(&mut io::stdout(), script.as_bytes())
        .map_err(|e| SkulkError::Validation(format!("Failed to write completions: {e}")))
}

/// Hand-rolled dynamic-completion snippet, appended verbatim to the static
/// `clap_complete` output.
fn dynamic_snippet(shell: CompletionShell) -> String {
    let cmds_pipe = AGENT_COMMANDS.join("|");
    match shell {
        CompletionShell::Bash => bash_snippet(&cmds_pipe),
        CompletionShell::Zsh => zsh_snippet(&cmds_pipe),
        CompletionShell::Fish => fish_snippet(),
    }
}

fn bash_snippet(cmds_pipe: &str) -> String {
    format!(
        r#"

# ── skulk: dynamic agent-name completion ────────────────────────────────
# Wraps the clap-generated _skulk function. When the user is completing
# the first positional argument after an agent-taking subcommand, we add
# live agent names from `skulk list`.
_skulk_with_agents() {{
    _skulk "$@"
    local cur="${{COMP_WORDS[COMP_CWORD]}}"
    local i subcmd="" subcmd_idx=-1
    for ((i=1; i<COMP_CWORD; i++)); do
        [[ "${{COMP_WORDS[i]}}" == -* ]] && continue
        subcmd="${{COMP_WORDS[i]}}"
        subcmd_idx=$i
        break
    done
    if [[ $subcmd_idx -ge 0 && $COMP_CWORD -eq $((subcmd_idx + 1)) ]]; then
        case "$subcmd" in
            {cmds_pipe})
                local agents
                agents=$(command skulk --no-color list 2>/dev/null \
                    | awk 'NR>1 && $1 ~ /^[a-z0-9][a-z0-9-]*$/ {{print $1}}')
                local a
                while IFS= read -r a; do
                    [[ -n "$a" && "$a" == "$cur"* ]] && COMPREPLY+=("$a")
                done <<< "$agents"
                ;;
        esac
    fi
}}
complete -F _skulk_with_agents -o bashdefault -o default skulk
"#
    )
}

fn zsh_snippet(cmds_pipe: &str) -> String {
    format!(
        r#"

# ── skulk: dynamic agent-name completion ────────────────────────────────
# Replaces compdef's binding with a wrapper that injects live agent
# names when completing the first positional after an agent-taking
# subcommand. Falls through to the clap-generated _skulk otherwise.
_skulk_agents() {{
    local -a agents
    agents=("${{(@f)$(command skulk --no-color list 2>/dev/null \
        | awk 'NR>1 && $1 ~ /^[a-z0-9][a-z0-9-]*$/ {{print $1}}')}}")
    _describe 'agent' agents
}}

_skulk_with_agents() {{
    local i subcmd="" subcmd_idx=0
    for ((i=2; i<=CURRENT; i++)); do
        [[ "${{words[i]}}" == -* ]] && continue
        subcmd="${{words[i]}}"
        subcmd_idx=$i
        break
    done
    if [[ $subcmd_idx -gt 0 && $CURRENT -eq $((subcmd_idx + 1)) ]]; then
        case "$subcmd" in
            {cmds_pipe})
                _skulk_agents
                return
                ;;
        esac
    fi
    _skulk "$@"
}}
compdef _skulk_with_agents skulk
"#
    )
}

fn fish_snippet() -> String {
    use std::fmt::Write as _;

    let mut out = String::from(
        "

# ── skulk: dynamic agent-name completion ────────────────────────────────
function __skulk_agents
    command skulk --no-color list 2>/dev/null \\
        | awk 'NR>1 && $1 ~ /^[a-z0-9][a-z0-9-]*$/ {print $1}'
end

",
    );
    for cmd in AGENT_COMMANDS {
        // writeln! to a String cannot fail; discard the Result.
        let _ = writeln!(
            out,
            "complete -c skulk -n \"__fish_seen_subcommand_from {cmd}\" -f -a \"(__skulk_agents)\""
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn bash_output_contains_static_and_dynamic_sections() {
        let out = render(CompletionShell::Bash);
        // Static section — clap emits a _skulk() function and knows about our subcommands.
        assert!(out.contains("_skulk()"), "missing clap static function");
        assert!(
            out.contains("connect"),
            "missing subcommand in static output"
        );
        // Dynamic section markers.
        assert!(
            out.contains("skulk: dynamic agent-name completion"),
            "missing dynamic header"
        );
        assert!(
            out.contains("_skulk_with_agents"),
            "missing wrapper function"
        );
        assert!(
            out.contains("command skulk --no-color list"),
            "missing dynamic list call"
        );
    }

    #[test]
    fn zsh_output_contains_static_and_dynamic_sections() {
        let out = render(CompletionShell::Zsh);
        assert!(out.contains("#compdef skulk"), "missing zsh compdef marker");
        assert!(out.contains("_skulk"), "missing clap static function");
        assert!(
            out.contains("skulk: dynamic agent-name completion"),
            "missing dynamic header"
        );
        assert!(out.contains("_skulk_agents"), "missing agents helper");
        assert!(
            out.contains("compdef _skulk_with_agents skulk"),
            "missing compdef wrapper registration"
        );
    }

    #[test]
    fn fish_output_contains_static_and_dynamic_sections() {
        let out = render(CompletionShell::Fish);
        // Fish output uses `complete -c skulk` rules.
        assert!(out.contains("complete -c skulk"), "missing clap fish rules");
        assert!(
            out.contains("skulk: dynamic agent-name completion"),
            "missing dynamic header"
        );
        assert!(
            out.contains("function __skulk_agents"),
            "missing fish agents function"
        );
        assert!(
            out.contains("__fish_seen_subcommand_from connect"),
            "missing per-command dynamic rule"
        );
    }

    #[test]
    fn dynamic_snippet_references_every_agent_command() {
        for shell in [
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
        ] {
            let out = render(shell);
            for cmd in AGENT_COMMANDS {
                assert!(
                    out.contains(cmd),
                    "{shell:?} snippet missing agent command '{cmd}'"
                );
            }
        }
    }

    #[test]
    fn non_agent_commands_are_not_in_dynamic_list() {
        // `new` takes a brand-new agent name — completing with existing names
        // would be confusing. Make sure it's excluded from the dynamic list.
        assert!(!AGENT_COMMANDS.contains(&"new"));
        assert!(!AGENT_COMMANDS.contains(&"list"));
        assert!(!AGENT_COMMANDS.contains(&"destroy-all"));
        assert!(!AGENT_COMMANDS.contains(&"gc"));
        assert!(!AGENT_COMMANDS.contains(&"init"));
        assert!(!AGENT_COMMANDS.contains(&"doctor"));
        assert!(!AGENT_COMMANDS.contains(&"pull"));
        assert!(!AGENT_COMMANDS.contains(&"completions"));
    }

    #[test]
    fn every_agent_command_is_a_real_subcommand() {
        // Guard against `AGENT_COMMANDS` drifting from the actual `Commands`
        // enum. If someone renames `destroy` → `remove`, static completion
        // keeps working (clap regenerates it) but the dynamic case arm would
        // silently stop firing. This test catches that at `cargo test` time.
        let cmd = Cli::command();
        let real: std::collections::HashSet<String> = cmd
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        for entry in AGENT_COMMANDS {
            assert!(
                real.contains(*entry),
                "AGENT_COMMANDS has stale entry '{entry}' — no such subcommand. \
                 Did you rename or remove a command? Update AGENT_COMMANDS in completions.rs."
            );
        }
    }

    #[test]
    fn completions_rejects_invalid_shell() {
        // clap's ValueEnum should refuse anything outside {bash, zsh, fish}
        // with a clear error pointing at possible values.
        // Not using expect_err() because Cli doesn't impl Debug.
        let err = match Cli::try_parse_from(["skulk", "completions", "powershell"]) {
            Ok(_) => panic!("expected parse error for unsupported shell"),
            Err(e) => e,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("bash") && rendered.contains("zsh") && rendered.contains("fish"),
            "error should list valid shells, got: {rendered}"
        );
    }

    #[test]
    fn completions_requires_shell_argument() {
        let result = Cli::try_parse_from(["skulk", "completions"]);
        assert!(
            result.is_err(),
            "expected clap error when no shell argument is provided"
        );
    }
}
