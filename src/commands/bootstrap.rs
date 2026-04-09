use std::collections::HashMap;

use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::shell_escape;

/// Build the SSH command to check which required tools are installed on the remote server.
pub(crate) fn bootstrap_check_command(cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let worktree_base = &cfg.worktree_base;
    format!(
        "for tool in tmux git; do \
            if command -v $tool >/dev/null 2>&1; then \
                echo \"$tool:installed\"; \
            else \
                echo \"$tool:missing\"; \
            fi; \
         done && \
         if command -v claude >/dev/null 2>&1 || [ -x ~/.local/bin/claude ]; then \
            echo \"claude:installed\"; \
         else \
            echo \"claude:missing\"; \
         fi && \
         if [ -d {base_path}/.git ]; then \
            echo \"repo:cloned\"; \
         else \
            echo \"repo:missing\"; \
         fi && \
         if [ -d {worktree_base} ]; then \
            echo \"worktree-dir:exists\"; \
         else \
            echo \"worktree-dir:missing\"; \
         fi"
    )
}

/// Parse the output of `bootstrap_check_command` into a map of component -> status.
pub(crate) fn parse_bootstrap_status(output: &str) -> HashMap<String, String> {
    let mut status = HashMap::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            status.insert(key.to_string(), value.to_string());
        }
    }
    status
}

/// Build the SSH command to install a tool via apt (Debian/Ubuntu assumed).
pub(crate) fn bootstrap_install_command(tool: &str) -> String {
    match tool {
        "tmux" => "sudo apt-get update -qq && sudo apt-get install -y -qq tmux".to_string(),
        "git" => "sudo apt-get update -qq && sudo apt-get install -y -qq git".to_string(),
        "claude" => "curl -fsSL https://claude.ai/install.sh | sh".to_string(),
        _ => format!("echo 'Unknown tool: {tool}'"),
    }
}

/// Build the SSH command to clone the repo on the remote server.
fn bootstrap_clone_command(repo_url: &str, cfg: &Config) -> String {
    let escaped = shell_escape(repo_url);
    let base_path = &cfg.base_path;
    format!("git clone '{escaped}' {base_path}")
}

/// Build the SSH command to create the worktree base directory.
fn bootstrap_create_worktree_dir_command(cfg: &Config) -> String {
    let worktree_base = &cfg.worktree_base;
    format!("mkdir -p {worktree_base}")
}

pub(crate) fn cmd_bootstrap(
    ssh: &impl Ssh,
    repo_url: &str,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let host = &cfg.host;
    let base_path = &cfg.base_path;
    let worktree_base = &cfg.worktree_base;

    eprintln!("Checking {host} setup...\n");

    // Step 1: Check what's installed
    let raw = ssh.run(&bootstrap_check_command(cfg))?;
    let status = parse_bootstrap_status(&raw);

    let tools = ["tmux", "git", "claude"];
    let mut printed_status = false;

    for tool in &tools {
        let state = status.get(*tool).map_or("unknown", String::as_str);
        match state {
            "installed" => {
                eprintln!("  {tool}: already installed");
                printed_status = true;
            }
            "missing" => {
                eprintln!("  {tool}: missing -- installing...");
                match ssh.run(&bootstrap_install_command(tool)) {
                    Ok(_) => eprintln!("  {tool}: installed successfully"),
                    Err(e) => {
                        eprintln!("  {tool}: installation failed -- {e}");
                        return Err(SkulkError::SshFailed(format!(
                            "Failed to install {tool} on {host}"
                        )));
                    }
                }
            }
            other => {
                eprintln!("  {tool}: unknown status ({other})");
            }
        }
    }

    if printed_status {
        eprintln!();
    }

    // Step 2: Clone repo if needed
    let repo_state = status.get("repo").map_or("unknown", String::as_str);
    match repo_state {
        "cloned" => {
            eprintln!("  repo: already cloned at {base_path}");
        }
        "missing" => {
            eprintln!("  repo: cloning {repo_url} to {base_path}...");
            ssh.run(&bootstrap_clone_command(repo_url, cfg))?;
            eprintln!("  repo: cloned successfully");
        }
        other => {
            eprintln!("  repo: unknown status ({other})");
        }
    }

    // Step 3: Create worktree base directory if needed
    let wt_state = status.get("worktree-dir").map_or("unknown", String::as_str);
    match wt_state {
        "exists" => {
            eprintln!("  worktree dir: already exists at {worktree_base}");
        }
        "missing" => {
            eprintln!("  worktree dir: creating {worktree_base}...");
            ssh.run(&bootstrap_create_worktree_dir_command(cfg))?;
            eprintln!("  worktree dir: created successfully");
        }
        other => {
            eprintln!("  worktree dir: unknown status ({other})");
        }
    }

    eprintln!("\nBootstrap complete. {host} is ready for `skulk new`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, test_config};

    #[test]
    fn bootstrap_check_command_checks_tmux() {
        let cfg = test_config();
        let cmd = bootstrap_check_command(&cfg);
        assert!(cmd.contains("tmux"));
    }

    #[test]
    fn bootstrap_check_command_checks_git() {
        let cfg = test_config();
        let cmd = bootstrap_check_command(&cfg);
        assert!(cmd.contains("git"));
    }

    #[test]
    fn bootstrap_check_command_checks_claude() {
        let cfg = test_config();
        let cmd = bootstrap_check_command(&cfg);
        assert!(cmd.contains("claude"));
    }

    #[test]
    fn bootstrap_check_command_checks_repo() {
        let cfg = test_config();
        let cmd = bootstrap_check_command(&cfg);
        assert!(cmd.contains(&*cfg.base_path));
    }

    #[test]
    fn bootstrap_check_command_checks_worktree_dir() {
        let cfg = test_config();
        let cmd = bootstrap_check_command(&cfg);
        assert!(cmd.contains(&*cfg.worktree_base));
    }

    #[test]
    fn parse_bootstrap_status_all_installed() {
        let output =
            "tmux:installed\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists\n";
        let status = parse_bootstrap_status(output);
        assert_eq!(status.get("tmux").unwrap(), "installed");
        assert_eq!(status.get("git").unwrap(), "installed");
        assert_eq!(status.get("claude").unwrap(), "installed");
        assert_eq!(status.get("repo").unwrap(), "cloned");
        assert_eq!(status.get("worktree-dir").unwrap(), "exists");
    }

    #[test]
    fn parse_bootstrap_status_some_missing() {
        let output =
            "tmux:installed\ngit:missing\nclaude:missing\nrepo:missing\nworktree-dir:missing\n";
        let status = parse_bootstrap_status(output);
        assert_eq!(status.get("tmux").unwrap(), "installed");
        assert_eq!(status.get("git").unwrap(), "missing");
    }

    #[test]
    fn parse_bootstrap_status_empty() {
        let status = parse_bootstrap_status("");
        assert!(status.is_empty());
    }

    #[test]
    fn bootstrap_install_tmux() {
        let cmd = bootstrap_install_command("tmux");
        assert!(cmd.contains("apt-get") && cmd.contains("tmux"));
    }

    #[test]
    fn bootstrap_install_git() {
        let cmd = bootstrap_install_command("git");
        assert!(cmd.contains("apt-get") && cmd.contains("git"));
    }

    #[test]
    fn bootstrap_install_claude() {
        let cmd = bootstrap_install_command("claude");
        assert!(cmd.contains("curl") && cmd.contains("install"));
    }

    #[test]
    fn bootstrap_install_unknown_tool() {
        let cmd = bootstrap_install_command("unknown-tool");
        assert!(cmd.contains("Unknown tool"));
    }

    #[test]
    fn bootstrap_clone_command_generates_quoted_clone() {
        let cfg = test_config();
        let cmd = bootstrap_clone_command("https://github.com/user/repo.git", &cfg);
        assert!(cmd.contains("git clone 'https://github.com/user/repo.git'"));
        assert!(cmd.contains(&*cfg.base_path));
    }

    #[test]
    fn bootstrap_clone_command_escapes_shell_metacharacters() {
        let cfg = test_config();
        let cmd = bootstrap_clone_command("x; rm -rf ~", &cfg);
        assert!(cmd.contains("'x; rm -rf ~'"));
        assert!(!cmd.contains("git clone x;"));
    }

    #[test]
    fn bootstrap_create_worktree_dir_command_generates() {
        let cfg = test_config();
        let cmd = bootstrap_create_worktree_dir_command(&cfg);
        assert!(cmd.contains("mkdir -p") && cmd.contains(&*cfg.worktree_base));
    }

    #[test]
    fn bootstrap_status_detects_fully_configured() {
        let output =
            "tmux:installed\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists\n";
        let status = parse_bootstrap_status(output);
        assert!(!status.values().any(|v| v == "missing"));
    }

    #[test]
    fn bootstrap_status_detects_partial_setup() {
        let output =
            "tmux:installed\ngit:installed\nclaude:missing\nrepo:missing\nworktree-dir:missing\n";
        let status = parse_bootstrap_status(output);
        let missing: Vec<&String> = status
            .iter()
            .filter(|(_, v)| v.as_str() == "missing")
            .map(|(k, _)| k)
            .collect();
        assert_eq!(missing.len(), 3);
    }

    #[test]
    fn cmd_bootstrap_all_installed() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(
            "tmux:installed\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists"
                .into(),
        )]);
        assert!(cmd_bootstrap(&ssh, "https://example.com/repo.git", &cfg).is_ok());
    }

    #[test]
    fn cmd_bootstrap_with_missing_tools() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(
                "tmux:missing\ngit:installed\nclaude:missing\nrepo:missing\nworktree-dir:missing"
                    .into(),
            ),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
            Ok(String::new()),
        ]);
        assert!(cmd_bootstrap(&ssh, "https://example.com/repo.git", &cfg).is_ok());
    }

    #[test]
    fn cmd_bootstrap_tool_install_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok(
                "tmux:missing\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists"
                    .into(),
            ),
            Err(SkulkError::SshFailed("apt-get failed".into())),
        ]);
        let result = cmd_bootstrap(&ssh, "https://example.com/repo.git", &cfg);
        assert!(result.is_err());
    }

    #[test]
    fn cmd_bootstrap_unknown_tool_status() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(
            "tmux:weird_status\ngit:weird_status\nclaude:weird_status\nrepo:weird_status\nworktree-dir:weird_status"
                .into(),
        )]);
        assert!(cmd_bootstrap(&ssh, "https://example.com/repo.git", &cfg).is_ok());
    }

    #[test]
    fn cmd_bootstrap_mixed_unknown_and_known_status() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("tmux:installed\ngit:weird\nclaude:missing\nrepo:cloned\nworktree-dir:weird".into()),
            Ok(String::new()),
        ]);
        assert!(cmd_bootstrap(&ssh, "https://example.com/repo.git", &cfg).is_ok());
    }
}
