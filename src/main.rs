mod agent_ref;
mod commands;
mod config;
mod display;
mod error;
mod inventory;
mod io;
mod ssh;
mod timings;
mod util;

#[cfg(test)]
mod testutil;

use std::time::Duration;

use clap::{Parser, Subcommand};

use commands::{
    destroy, doctor, gc, interact, list, new, prompt_source, pull, restart, ship, status, wait,
};
use config::Config;
use error::SkulkError;
use ssh::Ssh;
use timings::Timings;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "skulk",
    version,
    about = "Manage a fleet of AI coding agents on your own servers",
    long_about = "Manage remote Claude Code agents running on a configured SSH server via tmux.\n\nAgents are isolated Claude Code instances, each with their own git worktree\nand tmux session. Create agents to work on tasks in parallel, monitor their\noutput, and send them new instructions.\n\nConfigure via .skulk/config.toml in your project directory."
)]
pub(crate) struct Cli {
    /// Disable colored output
    #[arg(long, global = true)]
    pub(crate) no_color: bool,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// List all running agents on the remote server
    ///
    /// Shows name, status, uptime, and worktree path for each agent.
    /// Agents are tmux sessions with the configured session prefix.
    List,

    /// Update the base clone on the remote server to latest main
    ///
    /// Runs git pull --ff-only on the base repository. Use --force to
    /// discard local changes and hard-reset to origin/main.
    Pull {
        /// Force-reset to origin/main (discards all local changes)
        #[arg(long)]
        force: bool,
    },

    /// Create a new agent with worktree isolation
    ///
    /// Spins up a Claude Code instance in its own tmux session and git worktree.
    /// Optionally loads an initial prompt from a GitHub issue (--github) or a
    /// local text file (--from).
    New {
        /// Agent name (lowercase letters, digits, and hyphens only)
        name: String,
        /// Load initial prompt from a GitHub issue by number
        ///
        /// Fetches the issue (title, body, and all comments) via `gh` on the
        /// remote, then wraps it into a prompt for the agent. Requires `gh`
        /// installed and authenticated on the remote. Cross-repo syntax like
        /// `owner/repo#123` is not supported yet.
        #[arg(long, value_name = "ISSUE_ID", conflicts_with = "from")]
        github: Option<String>,
        /// Load initial prompt from a local text file
        ///
        /// Reads the file locally, wraps its contents into a prompt, and sends
        /// it to the agent.
        #[arg(long, value_name = "FILE")]
        from: Option<std::path::PathBuf>,
        /// Launch Claude with --remote-control so the agent is reachable from the
        /// Claude Code mobile/web app. Off by default because it triggers an upstream
        /// idle-death bug; Skulk's own commands work via tmux directly.
        #[arg(long)]
        remote_control: bool,
        /// Model name passed through to Claude Code as `--model <name>`
        /// (e.g. `opus`, `sonnet`, `claude-opus-4-7`). Restricted to
        /// `[A-Za-z0-9._-]` — shell metacharacters are rejected.
        #[arg(long, value_name = "NAME")]
        model: Option<String>,
        /// Extra flags appended to the Claude Code launch command. The string is
        /// typed into the remote shell by tmux, so shell metacharacters (`$`,
        /// backticks, `;`, `(`, `)`, globs, whitespace) are re-evaluated by that
        /// shell. Pre-quote any value that must reach Claude literally, e.g.
        /// `--claude-args "--allowed-tools 'Bash(gh pr:*)'"`.
        #[arg(long, value_name = "ARGS")]
        claude_args: Option<String>,
    },

    /// Destroy a specific agent
    ///
    /// Kills the tmux session, removes the git worktree, and deletes the branch.
    /// Requires confirmation unless --force is passed.
    Destroy {
        /// Agent name to destroy
        name: String,
        /// Skip the confirmation prompt
        #[arg(long)]
        force: bool,
    },

    /// Destroy all agents at once
    ///
    /// Removes all agent sessions, worktrees, and branches including orphaned resources.
    /// Requires confirmation unless --force is passed.
    DestroyAll {
        /// Skip the confirmation prompt
        #[arg(long)]
        force: bool,
    },

    /// Set up skulk for this project
    ///
    /// Interactive wizard that creates .skulk/config.toml and optionally sets up
    /// the remote server (install tools, clone repo, create worktree dir).
    /// Run this first in any new project.
    Init,

    /// Verify the runtime environment is correctly set up
    ///
    /// Runs a sequence of pass/fail checks against the local config and the
    /// remote host (SSH connectivity, tmux, claude, gh, base clone, worktree
    /// directory). Useful for debugging setup issues without re-running
    /// `skulk init`. Exits non-zero if any check fails.
    Doctor,

    /// Clean up orphaned tmux sessions, worktrees, and branches
    ///
    /// Finds agent resources that are out of sync (e.g., a session without a
    /// worktree) and removes them. Use --dry-run to preview without cleaning.
    Gc {
        /// Show what would be cleaned without actually cleaning
        #[arg(long)]
        dry_run: bool,
    },

    /// Attach to an agent's live tmux session
    ///
    /// Opens an interactive terminal session. Detach with Ctrl+B then D.
    Connect {
        /// Agent name to connect to
        name: String,
    },

    /// Show git diff between the default branch and an agent's branch
    ///
    /// Runs `git diff <default_branch>...<session_prefix><name>` on the remote.
    /// Useful for reviewing an agent's changes without attaching.
    Diff {
        /// Agent name
        name: String,
        /// Show only a summary of changed files, insertions, and deletions
        #[arg(long, conflicts_with = "name_only")]
        stat: bool,
        /// Show only the paths of changed files
        #[arg(long)]
        name_only: bool,
    },

    /// Detach all clients from an agent's tmux session
    ///
    /// Useful when an agent is attached from another terminal and you can't
    /// reach the keyboard to detach with Ctrl+B D. The agent keeps running.
    Disconnect {
        /// Agent name to detach clients from
        name: String,
    },

    /// View an agent's terminal output
    ///
    /// Shows a snapshot of the agent's current terminal by default.
    /// Use --follow for real-time updates or --lines for scrollback history.
    Logs {
        /// Agent name
        name: String,
        /// Follow output in real-time (like tail -f)
        #[arg(short, long)]
        follow: bool,
        /// Number of scrollback lines to show (default: visible pane only)
        #[arg(short, long)]
        lines: Option<u32>,
    },

    /// Send a prompt to a running agent
    ///
    /// Delivers the prompt text to the agent's Claude Code instance
    /// and verifies delivery via pane content comparison. Pass the prompt
    /// positionally, or use `--from <file>` to load contents from a local
    /// text file (useful for multi-line follow-ups or code snippets).
    Send {
        /// Agent name
        name: String,
        /// The prompt text to send (omit when using --from)
        #[arg(required_unless_present = "from")]
        prompt: Option<String>,
        /// Load prompt text from a local file
        ///
        /// Reads the file locally and sends its contents verbatim — no
        /// task-assignment wrapper. Trailing newlines are trimmed; internal
        /// whitespace is preserved.
        #[arg(long, value_name = "FILE", conflicts_with = "prompt")]
        from: Option<std::path::PathBuf>,
    },

    /// Push an agent's branch to `origin`
    ///
    /// Runs `git push -u origin <session_prefix><name>` on the remote,
    /// setting upstream tracking so subsequent pushes need no arguments.
    Push {
        /// Agent name
        name: String,
    },

    /// Archive an agent — kill its tmux session but keep worktree and branch intact
    ///
    /// Non-destructive alternative to `destroy`. Stops an agent that's done
    /// (or off the rails) without losing its work. Review the branch with
    /// `skulk diff` or inspect the worktree directly on the remote.
    Archive {
        /// Agent name to archive
        name: String,
    },

    /// Restart an agent in its existing worktree with a fresh Claude session
    ///
    /// Spins up a new tmux session running Claude in the agent's existing
    /// worktree — useful after `skulk archive`, or when a session has crashed
    /// or been killed. Claude starts with empty context; use `skulk send` or
    /// `claude --continue` inside the session to resume prior work.
    Restart {
        /// Agent name to restart
        name: String,
    },

    /// Show `git log` of commits on an agent's branch not in the default branch
    ///
    /// Runs `git log <default_branch>..<session_prefix><name> --oneline` on the remote.
    /// Named `git-log` (not `log`) to avoid collision with `logs`, which shows tmux output.
    GitLog {
        /// Agent name
        name: String,
    },

    /// Push an agent's branch and open a PR with a Claude-authored description
    ///
    /// Pushes `<session_prefix><name>` to `origin`, then opens a PR via `gh pr create`
    /// with a description authored by `claude -p` from the diff against the default
    /// branch. Requires `gh` and `claude` on the remote -- detected with a clean
    /// diagnostic if either is missing.
    Ship {
        /// Agent name
        name: String,
    },

    /// Dump an agent's full tmux scrollback for archive or review
    ///
    /// Captures all available scrollback (bounded by tmux's history-limit).
    /// Prints to stdout by default, or writes to a file with --output. Use
    /// this when you want the complete session history, not just recent
    /// activity (see `skulk logs` for that).
    Transcript {
        /// Agent name
        name: String,
        /// Write transcript to this file instead of stdout
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
    },

    /// Show detailed status for a single agent
    ///
    /// Fleet view is `skulk list`; this is the deep look — state, uptime,
    /// branch, commits ahead of the default branch, diff summary, and
    /// worktree path, all from a single SSH roundtrip.
    Status {
        /// Agent name
        name: String,
    },

    /// Block until an agent has finished its current turn
    ///
    /// Polls a marker file maintained by Claude Code `Stop` and
    /// `UserPromptSubmit` hooks installed at agent creation. Returns once the
    /// agent reports `idle`. Use `--all` to wait for every running agent.
    Wait {
        /// Agent name to wait for (omit when using --all)
        #[arg(required_unless_present = "all")]
        name: Option<String>,
        /// Wait for every running agent on the host
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Maximum seconds to wait before giving up (applies per agent with --all)
        #[arg(long, value_name = "SECS", default_value_t = 1800)]
        timeout: u64,
    },
}

impl Commands {
    /// CLI-facing name of the subcommand, used to tag dispatch errors in `run()`.
    ///
    /// Kept colocated with `Commands` (rather than derived in `run()`) so adding
    /// a new variant touches only this impl plus the dispatch match arm — not a
    /// parallel string table that can silently drift.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Commands::Init => "init",
            Commands::Doctor => "doctor",
            Commands::List => "list",
            Commands::Pull { .. } => "pull",
            Commands::New { .. } => "new",
            Commands::Destroy { .. } => "destroy",
            Commands::DestroyAll { .. } => "destroy-all",
            Commands::Gc { .. } => "gc",
            Commands::Connect { .. } => "connect",
            Commands::Diff { .. } => "diff",
            Commands::Disconnect { .. } => "disconnect",
            Commands::Logs { .. } => "logs",
            Commands::Send { .. } => "send",
            Commands::Push { .. } => "push",
            Commands::Archive { .. } => "archive",
            Commands::Restart { .. } => "restart",
            Commands::GitLog { .. } => "git-log",
            Commands::Ship { .. } => "ship",
            Commands::Status { .. } => "status",
            Commands::Transcript { .. } => "transcript",
            Commands::Wait { .. } => "wait",
        }
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    io::main();
}

// Explicitly allow `too_many_lines`: `run` is the central CLI dispatcher and
// each subcommand is a small, self-contained arm. Splitting it into helpers
// per command would spread a straightforward match across the module without
// reducing complexity.
#[allow(clippy::too_many_lines)]
pub(crate) fn run(
    cli: Cli,
    ssh: &impl Ssh,
    cfg: &Config,
    confirm: &dyn Fn(&str) -> bool,
    timings: &Timings,
) -> Result<(), (String, SkulkError)> {
    let cmd_name = cli.command.name();

    let result = match cli.command {
        Commands::Init => unreachable!(),
        Commands::Doctor => doctor::cmd_doctor(ssh, cfg),
        Commands::List => list::cmd_list(ssh, cfg),
        Commands::Pull { force } => pull::cmd_pull(ssh, force, cfg),
        Commands::New {
            name,
            github,
            from,
            remote_control,
            model,
            claude_args,
        } => {
            let env_file = new::resolve_local_env_file(cfg);
            new::cmd_new(
                ssh,
                &name,
                github.as_deref(),
                from.as_deref(),
                remote_control,
                model.as_deref(),
                claude_args.as_deref(),
                cfg,
                env_file.as_deref(),
            )
        }
        Commands::Destroy { name, force } => destroy::cmd_destroy(ssh, &name, force, cfg, confirm),
        Commands::DestroyAll { force } => destroy::cmd_destroy_all(ssh, force, cfg, confirm),
        Commands::Gc { dry_run } => gc::cmd_gc(ssh, dry_run, cfg),
        Commands::Connect { name } => interact::cmd_connect(ssh, &name, cfg),
        Commands::Diff {
            name,
            stat,
            name_only,
        } => {
            let format = match (stat, name_only) {
                (true, _) => interact::DiffFormat::Stat,
                (_, true) => interact::DiffFormat::NameOnly,
                _ => interact::DiffFormat::Default,
            };
            interact::cmd_diff(ssh, &name, format, cfg)
        }
        Commands::Disconnect { name } => interact::cmd_disconnect(ssh, &name, cfg),
        Commands::Logs {
            name,
            follow,
            lines,
        } => interact::cmd_logs(ssh, &name, follow, lines, cfg),
        Commands::Send { name, prompt, from } => {
            // clap's `required_unless_present`/`conflicts_with` make None+None
            // and Some+Some unreachable from the CLI, but we surface a
            // Validation error rather than panic to keep the contract
            // type-safe for non-CLI callers (mirrors the Wait arm above).
            let resolved = match (prompt, from) {
                (None, Some(path)) => prompt_source::load_file_raw(&path),
                (Some(p), None) => Ok(p),
                (Some(_), Some(_)) => Err(SkulkError::Validation(
                    "--from conflicts with positional prompt; pass only one".into(),
                )),
                (None, None) => Err(SkulkError::Validation(
                    "must provide prompt text or --from <file>".into(),
                )),
            };
            resolved
                .and_then(|p| interact::cmd_send(ssh, &name, &p, cfg, timings.send_verify_delay))
        }
        Commands::Push { name } => interact::cmd_push(ssh, &name, cfg),
        Commands::Archive { name } => interact::cmd_archive(ssh, &name, cfg),
        Commands::Restart { name } => restart::cmd_restart(ssh, &name, cfg),
        Commands::GitLog { name } => interact::cmd_git_log(ssh, &name, cfg),
        Commands::Ship { name } => ship::cmd_ship(ssh, &name, cfg),
        Commands::Status { name } => status::cmd_status(ssh, &name, cfg),
        Commands::Transcript { name, output } => {
            interact::cmd_transcript(ssh, &name, output.as_deref(), cfg)
        }
        Commands::Wait { name, all, timeout } => {
            let timeout = Duration::from_secs(timeout);
            if all {
                wait::cmd_wait_all(ssh, cfg, timings.wait_poll_interval, timeout)
            } else {
                // clap's `required_unless_present = "all"` makes `None` unreachable,
                // but we return a validation error rather than panic to keep the
                // contract type-safe instead of trusting clap's runtime invariant.
                match name {
                    Some(n) => wait::cmd_wait(ssh, &n, cfg, timings.wait_poll_interval, timeout),
                    None => Err(SkulkError::Validation(
                        "must specify an agent name or --all".into(),
                    )),
                }
            }
        }
    };

    result.map_err(|e| (cmd_name.to_string(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, mock_empty_inventory, mock_inventory, mock_inventory_single_agent,
        mock_list_output, mock_status_output, ssh_ok, test_config,
    };

    fn confirm_yes(_: &str) -> bool {
        true
    }

    #[test]
    fn run_dispatches_list() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_list_output(1_700_000_000, "", &[]))]);
        let cli = Cli {
            no_color: true,
            command: Commands::List,
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_pull() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("exists".into()), Ok("Already up to date.".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Pull { force: false },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_new() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            ssh_ok(),
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::New {
                name: "test".into(),
                github: None,
                from: None,
                remote_control: false,
                model: None,
                claude_args: None,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_destroy_force() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory_single_agent("skulk-target")),
            ssh_ok(), // kill session
            ssh_ok(), // worktree remove
            ssh_ok(), // branch delete
            ssh_ok(), // rm state file
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::Destroy {
                name: "target".into(),
                force: true,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_destroy_all_force() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let cli = Cli {
            no_color: true,
            command: Commands::DestroyAll { force: true },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_gc() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory_single_agent("skulk-healthy"))]);
        let cli = Cli {
            no_color: true,
            command: Commands::Gc { dry_run: true },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_connect() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);
        let cli = Cli {
            no_color: true,
            command: Commands::Connect {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_diff() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("diff output".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Diff {
                name: "test".into(),
                stat: false,
                name_only: false,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_diff_stat() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(" foo.rs | 2 +-".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Diff {
                name: "test".into(),
                stat: true,
                name_only: false,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_diff_name_only() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("foo.rs".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Diff {
                name: "test".into(),
                stat: false,
                name_only: true,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn diff_flags_stat_and_name_only_are_mutually_exclusive() {
        let result = Cli::try_parse_from(["skulk", "diff", "test", "--stat", "--name-only"]);
        assert!(
            result.is_err(),
            "expected clap conflict error when both --stat and --name-only are passed"
        );
    }

    #[test]
    fn new_github_and_from_are_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "skulk", "new", "agent", "--github", "42", "--from", "/tmp/x",
        ]);
        assert!(
            result.is_err(),
            "expected clap conflict error when both --github and --from are passed"
        );
    }

    #[test]
    fn new_no_longer_accepts_positional_prompt() {
        // The old `skulk new <name> <prompt>` form is removed; extra positional args should error.
        let result = Cli::try_parse_from(["skulk", "new", "agent", "fix the bug"]);
        assert!(
            result.is_err(),
            "positional prompt should no longer be accepted; use --from or --github"
        );
    }

    #[test]
    fn new_accepts_github_flag() {
        let cli = Cli::try_parse_from(["skulk", "new", "agent", "--github", "42"])
            .expect("parsing --github should succeed");
        match cli.command {
            Commands::New { github, from, .. } => {
                assert_eq!(github.as_deref(), Some("42"));
                assert!(from.is_none());
            }
            _ => panic!("expected Commands::New"),
        }
    }

    #[test]
    fn new_accepts_from_flag() {
        let cli = Cli::try_parse_from(["skulk", "new", "agent", "--from", "/tmp/task.txt"])
            .expect("parsing --from should succeed");
        match cli.command {
            Commands::New { github, from, .. } => {
                assert!(github.is_none());
                assert_eq!(
                    from.as_deref().and_then(|p| p.to_str()),
                    Some("/tmp/task.txt")
                );
            }
            _ => panic!("expected Commands::New"),
        }
    }

    #[test]
    fn run_dispatches_new_with_github_flag() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("SKULK_GH_OK".into()),
            Ok(r#"{"title":"T","body":"B","comments":[]}"#.into()),
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::New {
                name: "test".into(),
                github: Some("42".into()),
                from: None,
                remote_control: false,
                model: None,
                claude_args: None,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_new_with_from_flag() {
        use std::io::Write;
        let cfg = test_config();
        let tmp = std::env::temp_dir().join("skulk_main_from_test.txt");
        let mut f = std::fs::File::create(&tmp).unwrap();
        writeln!(f, "Do the thing.").unwrap();

        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_empty_inventory()),
            ssh_ok(),
            ssh_ok(),
            ssh_ok(),
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::New {
                name: "test".into(),
                github: None,
                from: Some(tmp.clone()),
                remote_control: false,
                model: None,
                claude_args: None,
            },
        };
        let result = run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero());
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_ok());
    }

    #[test]
    fn run_new_propagates_gh_missing_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("SKULK_GH_MISSING".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::New {
                name: "test".into(),
                github: Some("42".into()),
                from: None,
                remote_control: false,
                model: None,
                claude_args: None,
            },
        };
        let result = run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero());
        assert!(result.is_err());
        let (cmd, err) = result.unwrap_err();
        assert_eq!(cmd, "new");
        assert!(matches!(err, SkulkError::Diagnostic { .. }));
    }

    #[test]
    fn run_dispatches_disconnect() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);
        let cli = Cli {
            no_color: true,
            command: Commands::Disconnect {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_logs() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("some log output".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Logs {
                name: "test".into(),
                follow: false,
                lines: None,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_send() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("old pane".into()), ssh_ok(), Ok("new pane".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Send {
                name: "test".into(),
                prompt: Some("fix bug".into()),
                from: None,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn send_accepts_from_flag() {
        let cli = Cli::try_parse_from(["skulk", "send", "agent", "--from", "/tmp/follow-up.txt"])
            .expect("parsing --from should succeed");
        match cli.command {
            Commands::Send { prompt, from, .. } => {
                assert!(prompt.is_none());
                assert_eq!(
                    from.as_deref().and_then(|p| p.to_str()),
                    Some("/tmp/follow-up.txt")
                );
            }
            _ => panic!("expected Commands::Send"),
        }
    }

    #[test]
    fn send_prompt_and_from_are_mutually_exclusive() {
        let result = Cli::try_parse_from(["skulk", "send", "agent", "fix bug", "--from", "/tmp/x"]);
        assert!(
            result.is_err(),
            "expected clap conflict error when both prompt and --from are passed"
        );
    }

    #[test]
    fn send_requires_prompt_or_from() {
        let result = Cli::try_parse_from(["skulk", "send", "agent"]);
        assert!(
            result.is_err(),
            "expected clap error when neither prompt nor --from is provided"
        );
    }

    #[test]
    fn run_dispatches_send_with_from_flag() {
        use std::io::Write;
        let cfg = test_config();
        let tmp = std::env::temp_dir().join("skulk_main_send_from_test.txt");
        let mut f = std::fs::File::create(&tmp).unwrap();
        writeln!(f, "Take another look at PR #42.").unwrap();

        let ssh = MockSsh::new(vec![Ok("old pane".into()), ssh_ok(), Ok("new pane".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Send {
                name: "test".into(),
                prompt: None,
                from: Some(tmp.clone()),
            },
        };
        let result = run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero());
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_ok());
    }

    #[test]
    fn run_send_propagates_missing_file_error() {
        let cfg = test_config();
        // No SSH calls expected: the file read fails before we touch the wire.
        let ssh = MockSsh::new(vec![]);
        let cli = Cli {
            no_color: true,
            command: Commands::Send {
                name: "test".into(),
                prompt: None,
                from: Some(std::path::PathBuf::from(
                    "/nonexistent/skulk_send_absent.txt",
                )),
            },
        };
        let result = run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero());
        assert!(result.is_err());
        let (cmd, err) = result.unwrap_err();
        assert_eq!(cmd, "send");
        assert!(matches!(err, SkulkError::Validation(_)));
    }

    #[test]
    fn run_dispatches_push() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);
        let cli = Cli {
            no_color: true,
            command: Commands::Push {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_archive() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok()]);
        let cli = Cli {
            no_color: true,
            command: Commands::Archive {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_restart() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(mock_inventory(
                &[],
                &[("skulk-test", "/path/skulk-test")],
                &["skulk-test"],
            )),
            ssh_ok(),
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::Restart {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_git_log() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("abc1234 first commit".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::GitLog {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_ship() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            ssh_ok(),                                   // precheck
            ssh_ok(),                                   // push
            Ok("https://github.com/x/y/pull/1".into()), // gh pr create
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::Ship {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_status() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_status_output(
            1_700_000_200,
            "skulk-test\t1700000000\t1700000100\t0",
            &[("skulk-test", "/wt/skulk-test")],
            true,
            Some(1_700_000_150),
            Some(1),
            " 1 file changed, 5 insertions(+)",
        ))]);
        let cli = Cli {
            no_color: true,
            command: Commands::Status {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_transcript() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("full scrollback output".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Transcript {
                name: "test".into(),
                output: None,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_wait_single() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![ssh_ok(), Ok("idle".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Wait {
                name: Some("test".into()),
                all: false,
                timeout: 1800,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_dispatches_wait_all() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_empty_inventory())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Wait {
                name: None,
                all: true,
                timeout: 1800,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn wait_flags_name_and_all_are_mutually_exclusive() {
        let result = Cli::try_parse_from(["skulk", "wait", "test", "--all"]);
        assert!(
            result.is_err(),
            "expected clap conflict error when both name and --all are passed"
        );
    }

    #[test]
    fn wait_requires_name_or_all() {
        let result = Cli::try_parse_from(["skulk", "wait"]);
        assert!(
            result.is_err(),
            "expected clap error when neither name nor --all is provided"
        );
    }

    #[test]
    fn wait_accepts_name_only() {
        let result = Cli::try_parse_from(["skulk", "wait", "agent"]);
        assert!(result.is_ok(), "expected parse success");
    }

    #[test]
    fn wait_accepts_all_flag_only() {
        let result = Cli::try_parse_from(["skulk", "wait", "--all"]);
        assert!(result.is_ok(), "expected parse success");
    }

    #[test]
    fn run_dispatches_doctor() {
        let cfg = test_config();
        // Single SSH roundtrip — all checks pass.
        let probe = "tmux:installed:tmux 3.3a\n\
                     claude:installed:1.2.0\n\
                     gh:installed:gh version 2.40.1\n\
                     gh-auth:yes\n\
                     base:exists\n\
                     worktree:exists\n";
        let ssh = MockSsh::new(vec![Ok(probe.into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Doctor,
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero()).is_ok());
    }

    #[test]
    fn run_returns_error_with_command_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection timed out.".into(),
            suggestion: "Check network.".into(),
        })]);
        let cli = Cli {
            no_color: true,
            command: Commands::List,
        };
        let result = run(cli, &ssh, &cfg, &confirm_yes, &Timings::zero());
        assert!(result.is_err());
        let (cmd_name, _err) = result.unwrap_err();
        assert_eq!(cmd_name, "list");
    }
}
