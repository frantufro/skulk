mod commands;
mod config;
mod display;
mod error;
mod inventory;
mod io;
mod ssh;
mod util;

#[cfg(test)]
mod testutil;

use std::time::Duration;

use clap::{Parser, Subcommand};

use commands::{destroy, gc, interact, list, new, pull};
use config::Config;
use error::SkulkError;
use ssh::Ssh;

const SEND_VERIFY_DELAY: Duration = Duration::from_millis(500);

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "skulk",
    version,
    about = "Manage a fleet of AI coding agents on your own servers",
    long_about = "Manage remote Claude Code agents running on a configured SSH server via tmux.\n\nAgents are isolated Claude Code instances, each with their own git worktree\nand tmux session. Create agents to work on tasks in parallel, monitor their\noutput, and send them new instructions.\n\nConfigure via .skulk.toml in your project directory."
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
    /// Optionally sends an initial prompt to start working immediately.
    New {
        /// Agent name (lowercase letters, digits, and hyphens only)
        name: String,
        /// Initial prompt to send to the agent after startup
        prompt: Option<String>,
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
    /// Interactive wizard that creates .skulk.toml and optionally sets up
    /// the remote server (install tools, clone repo, create worktree dir).
    /// Run this first in any new project.
    Init,

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
    /// and verifies delivery via pane content comparison.
    Send {
        /// Agent name
        name: String,
        /// The prompt text to send
        prompt: String,
    },
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    io::main();
}

pub(crate) fn run(
    cli: Cli,
    ssh: &impl Ssh,
    cfg: &Config,
    confirm: &dyn Fn(&str) -> bool,
    send_verify_delay: Duration,
) -> Result<(), (String, SkulkError)> {
    let cmd_name = match &cli.command {
        Commands::Init => unreachable!("Init is handled before config loading"),
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
    };

    let result = match cli.command {
        Commands::Init => unreachable!(),
        Commands::List => list::cmd_list(ssh, cfg),
        Commands::Pull { force } => pull::cmd_pull(ssh, force, cfg),
        Commands::New { name, prompt } => new::cmd_new(ssh, &name, prompt.as_deref(), cfg),
        Commands::Destroy { name, force } => destroy::cmd_destroy(ssh, &name, force, cfg, confirm),
        Commands::DestroyAll { force } => destroy::cmd_destroy_all(ssh, force, cfg, confirm),
        Commands::Gc { dry_run } => gc::cmd_gc(ssh, dry_run, cfg),
        Commands::Connect { name } => interact::cmd_connect(ssh, &name, cfg),
        Commands::Diff { name } => interact::cmd_diff(ssh, &name, interact::DiffMode::Full, cfg),
        Commands::Disconnect { name } => interact::cmd_disconnect(ssh, &name, cfg),
        Commands::Logs {
            name,
            follow,
            lines,
        } => interact::cmd_logs(ssh, &name, follow, lines, cfg),
        Commands::Send { name, prompt } => {
            interact::cmd_send(ssh, &name, &prompt, cfg, send_verify_delay)
        }
    };

    result.map_err(|e| (cmd_name.to_string(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, mock_inventory, mock_list_output, test_config};

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
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_pull() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("exists".into()), Ok("Already up to date.".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Pull { force: false },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_new() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok(mock_inventory(&[], &[], &[])),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::New {
                name: "test".into(),
                prompt: None,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_destroy_force() {
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
        let cli = Cli {
            no_color: true,
            command: Commands::Destroy {
                name: "target".into(),
                force: true,
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_destroy_all_force() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(&[], &[], &[]))]);
        let cli = Cli {
            no_color: true,
            command: Commands::DestroyAll { force: true },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_gc() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_inventory(
            &["skulk-healthy"],
            &[("skulk-healthy", "/path/skulk-healthy")],
            &["skulk-healthy"],
        ))]);
        let cli = Cli {
            no_color: true,
            command: Commands::Gc { dry_run: true },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_connect() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Connect {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_diff() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("diff output".into())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Diff {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_disconnect() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        let cli = Cli {
            no_color: true,
            command: Commands::Disconnect {
                name: "test".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
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
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
    }

    #[test]
    fn run_dispatches_send() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("old pane".into()),
            Ok(String::new()),
            Ok("new pane".into()),
        ]);
        let cli = Cli {
            no_color: true,
            command: Commands::Send {
                name: "test".into(),
                prompt: "fix bug".into(),
            },
        };
        assert!(run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO).is_ok());
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
        let result = run(cli, &ssh, &cfg, &confirm_yes, Duration::ZERO);
        assert!(result.is_err());
        let (cmd_name, _err) = result.unwrap_err();
        assert_eq!(cmd_name, "list");
    }
}
