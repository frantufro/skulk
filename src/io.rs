//! System-boundary I/O code excluded from coverage reporting.
//!
//! This module contains code that directly interacts with external systems
//! (SSH processes, stdin, infinite polling loops, CLI entrypoint) and cannot
//! be meaningfully unit-tested without those systems present.
//!
//! All *logic* is kept in the other modules where it is tested via the injectable
//! `Ssh` trait and `MockSsh`. This module only contains the thin I/O wrappers
//! that bridge the trait to real system calls.
//!
//! Coverage tooling excludes `io.rs` files via `--ignore-filename-regex 'io\.rs$'`.

use std::process::Command as ProcessCommand;
use std::sync::atomic::Ordering;

use clap::Parser;

use crate::commands::init::{self, Prompter};
use crate::commands::interact::logs_snapshot_deep_command;
use crate::config::{self, Config, load_config};
use crate::display::checkmark;
use crate::display::{COLOR_ENABLED, use_color};
use crate::error::{SkulkError, classify_agent_error, classify_ssh_error};
use crate::ssh::Ssh;
use crate::util::{confirm_from_reader, find_new_content_start, is_localhost};
use crate::{Cli, Commands, run};

/// Read a yes/no confirmation from stdin.
pub(crate) fn confirm(prompt: &str) -> bool {
    let mut reader = std::io::BufReader::new(std::io::stdin());
    confirm_from_reader(prompt, &mut reader)
}

pub(crate) struct RealSsh {
    host: String,
}

impl Ssh for RealSsh {
    fn run(&self, cmd: &str) -> Result<String, SkulkError> {
        let local = is_localhost(&self.host);

        let output = if local {
            ProcessCommand::new("sh").args(["-c", cmd]).output()
        } else {
            ProcessCommand::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    &self.host,
                    cmd,
                ])
                .output()
        }
        .map_err(|e| {
            if !local && e.kind() == std::io::ErrorKind::NotFound {
                SkulkError::Diagnostic {
                    message: "ssh command not found.".into(),
                    suggestion: "Install OpenSSH.".into(),
                }
            } else {
                SkulkError::SshExec(e.to_string())
            }
        })?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Err(classify_ssh_error(&stderr, &self.host))
        }
    }

    /// Run a command interactively, inheriting stdin/stdout/stderr.
    ///
    /// Unlike `run()` which captures output, this function uses `Command::status()`
    /// for full terminal passthrough. Used by `connect` to attach to tmux sessions.
    ///
    /// For remote hosts:
    /// - Uses `-t` flag (force pseudo-terminal allocation for tmux)
    /// - Does NOT use `-o BatchMode=yes` (incompatible with interactive terminal)
    /// - Does NOT use `-o ConnectTimeout=10` (user expects to wait)
    ///
    /// For localhost: runs the command directly via `sh -c`.
    fn interactive(&self, cmd: &str) -> Result<std::process::ExitStatus, SkulkError> {
        let local = is_localhost(&self.host);

        let result = if local {
            ProcessCommand::new("sh").args(["-c", cmd]).status()
        } else {
            ProcessCommand::new("ssh")
                .args(["-t", &self.host, cmd])
                // Force xterm-256color because the default TERM inherited from the local shell
                // may not be installed on the remote (e.g., alacritty, ghostty), causing ncurses
                // errors inside tmux. xterm-256color is universally available.
                .env("TERM", "xterm-256color")
                .status()
        };

        result.map_err(|e| {
            if !local && e.kind() == std::io::ErrorKind::NotFound {
                SkulkError::Diagnostic {
                    message: "ssh command not found.".into(),
                    suggestion: "Install OpenSSH.".into(),
                }
            } else {
                SkulkError::SshExec(e.to_string())
            }
        })
    }
}

/// Follow agent output in real-time via poll loop.
pub(crate) fn cmd_logs_follow(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    let cmd = logs_snapshot_deep_command(name, 200, cfg);
    let initial = ssh
        .run(&cmd)
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    let mut last_lines: Vec<String> = initial.lines().map(ToString::to_string).collect();
    eprintln!("Following {name} (Ctrl+C to stop)...");
    for line in &last_lines {
        println!("{line}");
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match ssh.run(&cmd) {
            Ok(output) => {
                let current: Vec<String> = output.lines().map(ToString::to_string).collect();
                let new_start = find_new_content_start(&last_lines, &current);
                for line in &current[new_start..] {
                    println!("{line}");
                }
                last_lines = current;
            }
            Err(e) => {
                eprintln!("Warning: capture failed: {e}");
            }
        }
    }
}

// ── Init support ───────────────────────────────────────────────────────────

struct StdinPrompter;

impl Prompter for StdinPrompter {
    fn prompt(&mut self, message: &str) -> Result<String, SkulkError> {
        eprint!("{message}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| SkulkError::Validation(format!("Failed to read input: {e}")))?;
        Ok(line.trim().to_string())
    }

    fn confirm(&mut self, message: &str, default_yes: bool) -> Result<bool, SkulkError> {
        eprint!("{message} ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| SkulkError::Validation(format!("Failed to read input: {e}")))?;
        let answer = line.trim().to_lowercase();
        if answer.is_empty() {
            return Ok(default_yes);
        }
        Ok(answer == "y" || answer == "yes")
    }
}

/// Run a local command and return its stdout, or an error string.
fn run_local_command(cmd: &str) -> Result<String, String> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let output = ProcessCommand::new(parts[0])
        .args(&parts[1..])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn run_init() -> Result<(), SkulkError> {
    let color = use_color();

    // Print welcome banner
    eprintln!("{}", init::welcome_banner(color));

    // Detect git context
    let git_ctx = init::detect_git_context(&run_local_command);

    // Check if config exists
    let cwd = std::env::current_dir()
        .map_err(|e| SkulkError::Validation(format!("Cannot determine current directory: {e}")))?;
    let config_path = cwd.join(config::CONFIG_FILENAME);
    let config_exists = config_path.is_file();

    // SSH test closure
    let test_ssh = |host: &str| -> Result<(), SkulkError> {
        let ssh = RealSsh {
            host: host.to_string(),
        };
        ssh.run("echo ok").map(|_| ())
    };

    // Run wizard
    let mut prompter = StdinPrompter;
    let answers = init::run_wizard(&mut prompter, &git_ctx, config_exists, color, &test_ssh)?;

    // Write config
    let toml_content = init::generate_config_toml(&answers);
    std::fs::write(&config_path, toml_content).map_err(|e| {
        SkulkError::Validation(format!("Failed to write {}: {e}", config_path.display()))
    })?;
    eprintln!("\n  Writing .skulk.toml... {}", checkmark(color));

    // Remote setup if requested
    if answers.run_setup {
        let ssh = RealSsh {
            host: answers.host.clone(),
        };
        init::run_remote_setup(&ssh, &answers, color)?;
    }

    // Success
    eprintln!("{}", init::success_message(color));

    Ok(())
}

// ── Main ───────────────────────────────────────────────────────────────────

pub(crate) fn main() {
    let cli = Cli::parse();

    // Disable color if --no-color flag or NO_COLOR env var is set
    if cli.no_color || std::env::var("NO_COLOR").is_ok() {
        COLOR_ENABLED.store(false, Ordering::Relaxed);
    }

    // Init runs before config exists — handle it specially
    if matches!(cli.command, Commands::Init) {
        if let Err(e) = run_init() {
            if let SkulkError::InitAborted = e {
                eprintln!("Aborted.");
            } else {
                eprintln!("skulk init: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // All other commands require config
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cfg = match load_config(&cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skulk: {e}");
            std::process::exit(1);
        }
    };

    let ssh = RealSsh {
        host: cfg.host.clone(),
    };
    if let Err((cmd, e)) = run(cli, &ssh, &cfg, &confirm, crate::SEND_VERIFY_DELAY) {
        eprintln!("skulk {cmd}: {e}");
        std::process::exit(1);
    }
}
