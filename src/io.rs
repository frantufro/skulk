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

use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::atomic::Ordering;

use clap::Parser;

use crate::commands::completions;
use crate::commands::init::{self, InitOutcome, Prompter};
use crate::commands::interact::logs_snapshot_deep_command;
use crate::config::{self, Config, load_config};
use crate::display::checkmark;
use crate::display::{COLOR_ENABLED, use_color};
use crate::error::{SkulkError, classify_agent_error, classify_ssh_error};
use crate::ssh::Ssh;
use crate::util::{confirm_from_reader, shell_escape};
use crate::{Cli, Commands, run};

/// Check whether a host refers to the local machine.
///
/// When true, commands run locally via `sh -c` instead of over SSH.
/// Exact-match only: aliases like `localhost.localdomain`, `[::1]`, or
/// other 127.0.0.0/8 addresses are not recognized. Extend if users ask.
fn is_localhost(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Find the index in `new_lines` where new content begins, relative to `old_lines`.
///
/// Uses suffix-matching: finds the longest suffix of `old_lines` that appears as a
/// contiguous subsequence in `new_lines`. Everything after that match is new content.
/// Returns 0 if no overlap (show all), or `new_lines.len()` if nothing changed.
///
/// Complexity is O(n^2 * m) but bounded by the follow buffer size (200 lines).
fn find_new_content_start(old_lines: &[String], new_lines: &[String]) -> usize {
    if old_lines.is_empty() {
        return 0;
    }

    for start in 0..old_lines.len() {
        let suffix = &old_lines[start..];
        let suffix_len = suffix.len();

        if suffix_len <= new_lines.len() {
            for new_start in 0..=new_lines.len() - suffix_len {
                if new_lines[new_start..new_start + suffix_len] == *suffix {
                    return new_start + suffix_len;
                }
            }
        }
    }

    0
}

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
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            // classify_ssh_error looks for SSH-specific keywords (connection refused,
            // permission denied, host key) that would produce misleading suggestions
            // like "ssh localhost whoami" for local sh -c failures. Skip it for localhost.
            if local {
                Err(SkulkError::SshFailed(stderr.trim().to_string()))
            } else {
                Err(classify_ssh_error(&stderr, &self.host))
            }
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
            // No TERM override needed: the local terminfo matches $TERM by construction.
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

    fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<(), SkulkError> {
        let local = is_localhost(&self.host);

        let output = if local {
            // Route through `sh -c` so `~` in remote_path expands the same way
            // it does for every other localhost operation (all other commands
            // run via `sh -c`, which expands tildes; bare `cp` does not).
            // remote_path is validated shell-safe at config load time; local_path
            // is wrapped in single quotes via `shell_escape` to tolerate spaces.
            let local_str = local_path.to_string_lossy();
            let cmd = format!("cp '{}' {}", shell_escape(&local_str), remote_path);
            ProcessCommand::new("sh").args(["-c", &cmd]).output()
        } else {
            let dest = format!("{}:{}", self.host, remote_path);
            ProcessCommand::new("scp")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
                .arg(local_path)
                .arg(&dest)
                .output()
        }
        .map_err(|e| {
            if !local && e.kind() == std::io::ErrorKind::NotFound {
                SkulkError::Diagnostic {
                    message: "scp command not found.".into(),
                    suggestion: "Install OpenSSH.".into(),
                }
            } else {
                SkulkError::SshExec(e.to_string())
            }
        })?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if local {
                Err(SkulkError::SshFailed(stderr))
            } else {
                Err(classify_ssh_error(&stderr, &self.host))
            }
        }
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

fn run_init() -> Result<InitOutcome, SkulkError> {
    let color = use_color();

    // Print welcome banner
    eprintln!("{}", init::welcome_banner(color));

    // Detect git context
    let git_ctx = init::detect_git_context(&run_local_command);

    // Check if config exists (new layout or legacy file)
    let cwd = std::env::current_dir()
        .map_err(|e| SkulkError::Validation(format!("Cannot determine current directory: {e}")))?;
    let config_path = config::config_path_in(&cwd);
    let legacy_path = cwd.join(".skulk.toml");
    let config_exists = config_path.is_file() || legacy_path.is_file();

    // SSH test closure
    let test_ssh = |host: &str| -> Result<(), SkulkError> {
        let ssh = RealSsh {
            host: host.to_string(),
        };
        ssh.run("echo ok").map(|_| ())
    };

    // Run wizard
    let mut prompter = StdinPrompter;
    let Some(answers) = init::run_wizard(&mut prompter, &git_ctx, config_exists, color, &test_ssh)?
    else {
        return Ok(InitOutcome::Aborted);
    };

    // Write config under .skulk/
    let toml_content = init::generate_config_toml(&answers);
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            SkulkError::Validation(format!("Failed to create {}: {e}", parent.display()))
        })?;
    }
    std::fs::write(&config_path, toml_content).map_err(|e| {
        SkulkError::Validation(format!("Failed to write {}: {e}", config_path.display()))
    })?;
    eprintln!(
        "\n  Writing {}/{}... {}",
        config::CONFIG_DIR,
        config::CONFIG_FILENAME,
        checkmark(color)
    );
    if legacy_path.is_file() {
        match std::fs::remove_file(&legacy_path) {
            Ok(()) => eprintln!("  Removed legacy .skulk.toml."),
            Err(e) => eprintln!("  warning: failed to remove legacy .skulk.toml: {e}"),
        }
    }

    // Write .skulk/init.sh.example so users have a template to rename.
    let skulk_dir = cwd.join(".skulk");
    std::fs::create_dir_all(&skulk_dir).map_err(|e| {
        SkulkError::Validation(format!("Failed to create {}: {e}", skulk_dir.display()))
    })?;
    let example_path = skulk_dir.join("init.sh.example");
    if !example_path.exists() {
        std::fs::write(&example_path, init::init_script_example_content()).map_err(|e| {
            SkulkError::Validation(format!("Failed to write {}: {e}", example_path.display()))
        })?;
        eprintln!("  Writing .skulk/init.sh.example... {}", checkmark(color));
    }

    // Add .skulk/.env to local .gitignore so secrets don't get committed.
    // Distinguish "file doesn't exist" (treat as empty) from other read errors
    // (surface to the user) — otherwise a perm-denied read would silently be
    // treated as empty and the subsequent write could clobber the real file.
    let gitignore_path = cwd.join(".gitignore");
    let existing = match std::fs::read_to_string(&gitignore_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(SkulkError::Validation(format!(
                "Failed to read {}: {e}",
                gitignore_path.display()
            )));
        }
    };
    if let Some(updated) = init::ensure_gitignore_entry(&existing) {
        std::fs::write(&gitignore_path, updated).map_err(|e| {
            SkulkError::Validation(format!(
                "Failed to update {}: {e}",
                gitignore_path.display()
            ))
        })?;
        eprintln!(
            "  Updating .gitignore ({}) ... {}",
            init::GITIGNORE_ENV_ENTRY,
            checkmark(color)
        );
    }

    // Remote setup if requested
    if answers.run_setup {
        let ssh = RealSsh {
            host: answers.host.clone(),
        };
        init::run_remote_setup(&ssh, &answers, color)?;
    }

    // Success
    eprintln!("{}", init::success_message(color));

    Ok(InitOutcome::Done)
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
        match run_init() {
            Ok(InitOutcome::Done) => {}
            Ok(InitOutcome::Aborted) => eprintln!("Aborted."),
            Err(e) => {
                eprintln!("skulk init: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Completions are a pure static output — never touch config or SSH.
    if let Commands::Completions { shell } = cli.command {
        if let Err(e) = completions::cmd_completions(shell) {
            eprintln!("skulk completions: {e}");
            std::process::exit(1);
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
    if let Err((cmd, e)) = run(
        cli,
        &ssh,
        &cfg,
        &confirm,
        &crate::timings::Timings::production(),
    ) {
        eprintln!("skulk {cmd}: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_localhost tests ───────────────────────────────────────────────

    #[test]
    fn is_localhost_name() {
        assert!(is_localhost("localhost"));
    }

    #[test]
    fn is_localhost_ipv4_loopback() {
        assert!(is_localhost("127.0.0.1"));
    }

    #[test]
    fn is_localhost_ipv6_loopback() {
        assert!(is_localhost("::1"));
    }

    #[test]
    fn is_localhost_remote_host() {
        assert!(!is_localhost("myserver.example.com"));
    }

    // ── find_new_content_start tests ────────────────────────────────────

    #[test]
    fn find_new_content_start_partial_overlap() {
        let old = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let new = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        assert_eq!(find_new_content_start(&old, &new), 2);
    }

    #[test]
    fn find_new_content_start_no_change() {
        let old = vec!["a".to_string(), "b".to_string()];
        let new = vec!["a".to_string(), "b".to_string()];
        assert_eq!(find_new_content_start(&old, &new), 2);
    }

    #[test]
    fn find_new_content_start_complete_change() {
        let old = vec!["a".to_string(), "b".to_string()];
        let new = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        assert_eq!(find_new_content_start(&old, &new), 0);
    }

    #[test]
    fn find_new_content_start_empty_old() {
        let old: Vec<String> = vec![];
        let new = vec!["a".to_string(), "b".to_string()];
        assert_eq!(find_new_content_start(&old, &new), 0);
    }

    #[test]
    fn find_new_content_start_suffix_match_at_last_iteration() {
        let old = vec!["x".to_string(), "y".to_string(), "a".to_string()];
        let new = vec![
            "b".to_string(),
            "c".to_string(),
            "a".to_string(),
            "d".to_string(),
        ];
        assert_eq!(find_new_content_start(&old, &new), 3);
    }

    #[test]
    fn find_new_content_start_empty_new() {
        let old = vec!["a".to_string(), "b".to_string()];
        let new: Vec<String> = vec![];
        assert_eq!(find_new_content_start(&old, &new), 0);
    }

    #[test]
    fn find_new_content_start_partial_match_not_contiguous() {
        let old = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let new = vec![
            "a".to_string(),
            "x".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert_eq!(find_new_content_start(&old, &new), 3);
    }
}
