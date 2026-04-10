use std::collections::HashMap;

use crate::config::validate_shell_safe;
use crate::display::{bold, checkmark, crossmark, dim, green};
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::shell_escape;

// ── Types ──────────────────────────────────────────────────────────────────

/// Trait for injectable user prompting (stdin in production, scripted in tests).
pub(crate) trait Prompter {
    /// Show a prompt and return the user's trimmed input.
    fn prompt(&mut self, message: &str) -> Result<String, SkulkError>;

    /// Show a yes/no prompt. `default_yes` controls what Enter alone means.
    fn confirm(&mut self, message: &str, default_yes: bool) -> Result<bool, SkulkError>;
}

/// Git context auto-detected from the local repository.
#[derive(Debug, Clone, Default)]
pub(crate) struct GitContext {
    pub remote_url: Option<String>,
    pub default_branch: Option<String>,
    pub repo_name: Option<String>,
}

/// Collected answers from the interactive wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitAnswers {
    pub host: String,
    pub session_prefix: String,
    pub default_branch: String,
    pub base_path: String,
    pub worktree_base: String,
    pub repo_url: String,
    pub repo_name: String,
    pub run_setup: bool,
}

// ── Git context detection ──────────────────────────────────────────────────

/// Parse a repo name from a git remote URL.
///
/// Handles HTTPS (`https://github.com/user/repo.git`) and
/// SSH (`git@github.com:user/repo.git`) URLs.
pub(crate) fn parse_repo_name(url: &str) -> Option<String> {
    let url = url.trim();
    let url = url.strip_suffix(".git").unwrap_or(url);
    let url = url.strip_suffix('/').unwrap_or(url);

    // Try splitting by '/' first (HTTPS), then ':' (SSH)
    let name = url
        .rsplit_once('/')
        .map(|(_, name)| name)
        .or_else(|| url.rsplit_once(':').map(|(_, name)| name))?;

    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Detect git context from the local repository.
///
/// Uses the provided `run_local` callback to execute git commands.
/// Returns whatever could be detected — all fields are optional.
pub(crate) fn detect_git_context(run_local: &dyn Fn(&str) -> Result<String, String>) -> GitContext {
    let remote_url = run_local("git remote get-url origin").ok();

    let default_branch = run_local("git symbolic-ref refs/remotes/origin/HEAD")
        .ok()
        .and_then(|refstr| {
            refstr
                .strip_prefix("refs/remotes/origin/")
                .map(ToString::to_string)
        });

    let repo_name = remote_url.as_deref().and_then(parse_repo_name);

    GitContext {
        remote_url,
        default_branch,
        repo_name,
    }
}

// ── Wizard ─────────────────────────────────────────────────────────────────

/// Run the interactive init wizard, collecting all user answers.
///
/// All I/O is injected via `prompter` and `test_ssh`. The wizard handles:
/// - Config-exists check with reconfigure prompt
/// - Git context display and manual fallbacks
/// - SSH connectivity testing with retry
/// - Session prefix and branch with defaults
/// - Remote setup offer
pub(crate) fn run_wizard(
    prompter: &mut dyn Prompter,
    git_ctx: &GitContext,
    config_exists: bool,
    color: bool,
    test_ssh: &dyn Fn(&str) -> Result<(), SkulkError>,
) -> Result<InitAnswers, SkulkError> {
    // Step 1: Config already exists?
    if config_exists
        && !prompter.confirm(
            &format!(
                "  {} .skulk.toml already exists. Reconfigure?",
                dim("[y/N]", color)
            ),
            false,
        )?
    {
        return Err(SkulkError::InitAborted);
    }

    // Step 2: Determine repo URL and name
    let (repo_url, repo_name) = detect_repo_info(prompter, git_ctx, color)?;

    // Step 3: SSH host (required) + connectivity test
    let host = prompt_and_test_ssh(prompter, color, test_ssh)?;

    // Step 4: Session prefix
    let default_prefix = format!("{repo_name}-");
    let session_prefix = loop {
        let input = prompter.prompt(&format!(
            "{} Session prefix {}: ",
            green("?", color),
            dim(&format!("[{default_prefix}]"), color)
        ))?;
        let value = if input.is_empty() {
            default_prefix.clone()
        } else {
            input
        };
        if let Err(e) = validate_shell_safe(&value, "session_prefix") {
            eprintln!("  {e}");
            continue;
        }
        break value;
    };

    // Step 5: Default branch
    let detected_branch = git_ctx.default_branch.as_deref().unwrap_or("main");
    let default_branch = loop {
        let input = prompter.prompt(&format!(
            "{} Default branch {}: ",
            green("?", color),
            dim(&format!("[{detected_branch}]"), color)
        ))?;
        let value = if input.is_empty() {
            detected_branch.to_string()
        } else {
            input
        };
        if let Err(e) = validate_shell_safe(&value, "default_branch") {
            eprintln!("  {e}");
            continue;
        }
        break value;
    };

    // Step 6: Derive paths
    let base_path = format!("~/{repo_name}");
    let worktree_base = format!("~/{repo_name}-worktrees");

    // Step 7: Show config summary
    eprintln!();
    eprintln!("  {}", bold("Config:", color));
    eprintln!("    host           = {host}");
    eprintln!("    session_prefix = {session_prefix}");
    eprintln!("    base_path      = {base_path}");
    eprintln!("    worktree_base  = {worktree_base}");
    eprintln!("    default_branch = {default_branch}");

    // Step 8: Remote setup?
    let run_setup = prompter.confirm(
        &format!(
            "\n{} Set up {host} now? {}",
            green("?", color),
            dim("[Y/n]", color)
        ),
        true,
    )?;

    Ok(InitAnswers {
        host,
        session_prefix,
        default_branch,
        base_path,
        worktree_base,
        repo_url,
        repo_name,
        run_setup,
    })
}

/// Prompt for SSH host, validate, and test connectivity with retry.
fn prompt_and_test_ssh(
    prompter: &mut dyn Prompter,
    color: bool,
    test_ssh: &dyn Fn(&str) -> Result<(), SkulkError>,
) -> Result<String, SkulkError> {
    let host = loop {
        let input = prompter.prompt(&format!("\n{} SSH host: ", green("?", color)))?;
        if input.is_empty() {
            eprintln!("  SSH host cannot be empty.");
            continue;
        }
        if let Err(e) = validate_shell_safe(&input, "host") {
            eprintln!("  {e}");
            continue;
        }
        break input;
    };

    loop {
        match test_ssh(&host) {
            Ok(()) => {
                eprintln!("  {} Connected to {host}", checkmark(color));
                return Ok(host);
            }
            Err(e) => {
                eprintln!("  {} {e}", crossmark(color));
                if !prompter.confirm("  Retry? [Y/n]", true)? {
                    return Err(SkulkError::InitAborted);
                }
            }
        }
    }
}

/// Determine repo URL and name from git context or manual input.
fn detect_repo_info(
    prompter: &mut dyn Prompter,
    git_ctx: &GitContext,
    color: bool,
) -> Result<(String, String), SkulkError> {
    let repo_url = if let Some(ref url) = git_ctx.remote_url {
        eprintln!("  Detected git remote: {}", bold(url, color));
        url.clone()
    } else {
        eprintln!("  No git remote detected.");
        loop {
            let input = prompter.prompt(&format!("{} Git repo URL: ", green("?", color)))?;
            if input.is_empty() {
                eprintln!("  Repo URL cannot be empty.");
                continue;
            }
            break input;
        }
    };

    let repo_name = if let Some(ref name) = git_ctx.repo_name {
        name.clone()
    } else if let Some(parsed) = parse_repo_name(&repo_url) {
        eprintln!("  Derived repo name: {}", bold(&parsed, color));
        parsed
    } else {
        loop {
            let input = prompter.prompt(&format!("{} Repo name: ", green("?", color)))?;
            if input.is_empty() {
                eprintln!("  Repo name cannot be empty.");
                continue;
            }
            if let Err(e) = validate_shell_safe(&input, "repo_name") {
                eprintln!("  {e}");
                continue;
            }
            break input;
        }
    };

    // Validate repo_name even when auto-detected (it flows into shell commands and TOML)
    validate_shell_safe(&repo_name, "repo_name").map_err(SkulkError::Validation)?;

    Ok((repo_url, repo_name))
}

// ── Config generation ──────────────────────────────────────────────────────

/// Generate `.skulk.toml` content from wizard answers.
pub(crate) fn generate_config_toml(answers: &InitAnswers) -> String {
    format!(
        "host = \"{host}\"\n\
         session_prefix = \"{prefix}\"\n\
         base_path = \"{base}\"\n\
         worktree_base = \"{wt}\"\n\
         default_branch = \"{branch}\"\n",
        host = answers.host,
        prefix = answers.session_prefix,
        base = answers.base_path,
        wt = answers.worktree_base,
        branch = answers.default_branch,
    )
}

// ── Remote setup ───────────────────────────────────────────────────────────

/// Build the SSH command to check if apt-get is available.
pub(crate) fn check_apt_command() -> &'static str {
    "command -v apt-get"
}

/// Build the SSH command to check which tools and directories exist on the remote.
pub(crate) fn setup_check_command(answers: &InitAnswers) -> String {
    let base_path = &answers.base_path;
    let worktree_base = &answers.worktree_base;
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

/// Parse the output of `setup_check_command` into a map of component -> status.
pub(crate) fn parse_setup_status(output: &str) -> HashMap<String, String> {
    let mut status = HashMap::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            status.insert(key.to_string(), value.to_string());
        }
    }
    status
}

/// Build the SSH command to install a tool via apt (Debian/Ubuntu).
pub(crate) fn setup_install_command(tool: &str) -> String {
    match tool {
        "tmux" => "sudo apt-get update -qq && sudo apt-get install -y -qq tmux".to_string(),
        "git" => "sudo apt-get update -qq && sudo apt-get install -y -qq git".to_string(),
        "claude" => "curl -fsSL https://claude.ai/install.sh | sh".to_string(),
        _ => format!("echo 'Unknown tool: {tool}'"),
    }
}

/// Build the SSH command to clone the repo on the remote server.
pub(crate) fn setup_clone_command(repo_url: &str, base_path: &str) -> String {
    let escaped = shell_escape(repo_url);
    format!("git clone '{escaped}' {base_path}")
}

/// Build the SSH command to create the worktree base directory.
pub(crate) fn setup_create_worktree_dir_command(worktree_base: &str) -> String {
    format!("mkdir -p {worktree_base}")
}

/// Run the remote setup sequence: check for apt-get, detect installed tools,
/// install missing tools, clone repo, create worktree directory.
pub(crate) fn run_remote_setup(
    ssh: &impl Ssh,
    answers: &InitAnswers,
    color: bool,
) -> Result<(), SkulkError> {
    let host = &answers.host;

    eprintln!("\n  Setting up {host}...");

    // Step 1: Check for apt-get
    if ssh.run(check_apt_command()).is_err() {
        eprintln!("  {} apt-get not found on {host}.", crossmark(color));
        eprintln!("  Skulk currently only supports Debian/Ubuntu servers.");
        eprintln!("  Want support for your OS? Open an issue or PR:");
        eprintln!("  https://github.com/frantufro/skulk/issues");
        return Err(SkulkError::Validation(
            "apt-get not found. Debian/Ubuntu required.".into(),
        ));
    }

    // Step 2: Check what's installed
    let raw = ssh.run(&setup_check_command(answers))?;
    let status = parse_setup_status(&raw);

    // Step 3: Install missing tools
    let tools = ["tmux", "git", "claude"];
    for tool in &tools {
        let state = status.get(*tool).map_or("unknown", String::as_str);
        match state {
            "installed" => {
                eprintln!("  {} {tool} (already installed)", checkmark(color));
            }
            "missing" => {
                eprintln!("  \u{27f3} Installing {tool}...");
                match ssh.run(&setup_install_command(tool)) {
                    Ok(_) => eprintln!("  {} {tool} installed", checkmark(color)),
                    Err(e) => {
                        eprintln!("  {} {tool} install failed: {e}", crossmark(color));
                    }
                }
            }
            _ => {
                eprintln!("  ? {tool}: unknown status");
            }
        }
    }

    // Step 4: Clone repo if needed
    let repo_state = status.get("repo").map_or("unknown", String::as_str);
    match repo_state {
        "cloned" => {
            eprintln!(
                "  {} repo (already cloned at {})",
                checkmark(color),
                answers.base_path
            );
        }
        "missing" => {
            eprintln!("  \u{27f3} Cloning repository...");
            ssh.run(&setup_clone_command(&answers.repo_url, &answers.base_path))?;
            eprintln!(
                "  {} repo cloned to {}",
                checkmark(color),
                answers.base_path
            );
        }
        other => {
            eprintln!("  ? repo: unexpected status ({other})");
        }
    }

    // Step 5: Create worktree dir if needed
    let wt_state = status.get("worktree-dir").map_or("unknown", String::as_str);
    match wt_state {
        "exists" => {
            eprintln!("  {} worktree dir (already exists)", checkmark(color),);
        }
        "missing" => {
            ssh.run(&setup_create_worktree_dir_command(&answers.worktree_base))?;
            eprintln!("  {} worktree dir created", checkmark(color));
        }
        other => {
            eprintln!("  ? worktree dir: unexpected status ({other})");
        }
    }

    Ok(())
}

// ── Display ────────────────────────────────────────────────────────────────

/// Build the welcome banner.
pub(crate) fn welcome_banner(color: bool) -> String {
    if color {
        "\n\x1b[1m\u{1f43a} Welcome to Skulk!\x1b[0m\n   Let's set up your project.\n".to_string()
    } else {
        "\nWelcome to Skulk!\nLet's set up your project.\n".to_string()
    }
}

/// Build the success message with next steps.
pub(crate) fn success_message(answers: &InitAnswers, color: bool) -> String {
    let cmd = format!("skulk new {}", example_agent_name(&answers.repo_name));
    if color {
        format!("\n\x1b[1m\u{1f389} Ready!\x1b[0m Create your first agent with:\n   {cmd}\n")
    } else {
        format!("\nReady! Create your first agent with:\n   {cmd}\n")
    }
}

/// Generate an example agent name.
fn example_agent_name(_repo_name: &str) -> &'static str {
    "my-task"
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::testutil::{MockPrompter, MockSsh};

    fn mock_ssh_test_ok(_host: &str) -> Result<(), SkulkError> {
        Ok(())
    }

    fn mock_ssh_test_fail(_host: &str) -> Result<(), SkulkError> {
        Err(SkulkError::Diagnostic {
            message: "Connection refused.".into(),
            suggestion: "Check host.".into(),
        })
    }

    fn git_ctx_full() -> GitContext {
        GitContext {
            remote_url: Some("git@github.com:user/my-project.git".into()),
            default_branch: Some("main".into()),
            repo_name: Some("my-project".into()),
        }
    }

    fn git_ctx_empty() -> GitContext {
        GitContext::default()
    }

    // ── parse_repo_name ────────────────────────────────────────────────

    #[test]
    fn parse_repo_name_https_with_git_suffix() {
        assert_eq!(
            parse_repo_name("https://github.com/user/my-project.git"),
            Some("my-project".into())
        );
    }

    #[test]
    fn parse_repo_name_https_without_git_suffix() {
        assert_eq!(
            parse_repo_name("https://github.com/user/my-project"),
            Some("my-project".into())
        );
    }

    #[test]
    fn parse_repo_name_ssh_url() {
        assert_eq!(
            parse_repo_name("git@github.com:user/my-project.git"),
            Some("my-project".into())
        );
    }

    #[test]
    fn parse_repo_name_trailing_slash() {
        assert_eq!(
            parse_repo_name("https://github.com/user/my-project/"),
            Some("my-project".into())
        );
    }

    #[test]
    fn parse_repo_name_empty_returns_none() {
        assert_eq!(parse_repo_name(""), None);
    }

    #[test]
    fn parse_repo_name_just_slash_returns_none() {
        assert_eq!(parse_repo_name("/"), None);
    }

    #[test]
    fn parse_repo_name_ssh_no_path_separator() {
        assert_eq!(parse_repo_name("git@host:repo.git"), Some("repo".into()));
    }

    #[test]
    fn parse_repo_name_bare_name_returns_none() {
        // No '/' or ':' means rsplit_once returns None for both
        assert_eq!(parse_repo_name("just-a-name"), None);
    }

    // ── detect_git_context ─────────────────────────────────────────────

    #[test]
    fn detect_git_context_in_repo() {
        let ctx = detect_git_context(&|cmd| match cmd {
            "git remote get-url origin" => Ok("git@github.com:user/my-project.git".into()),
            "git symbolic-ref refs/remotes/origin/HEAD" => Ok("refs/remotes/origin/develop".into()),
            _ => Err("unknown".into()),
        });
        assert_eq!(
            ctx.remote_url.as_deref(),
            Some("git@github.com:user/my-project.git")
        );
        assert_eq!(ctx.default_branch.as_deref(), Some("develop"));
        assert_eq!(ctx.repo_name.as_deref(), Some("my-project"));
    }

    #[test]
    fn detect_git_context_not_in_repo() {
        let ctx = detect_git_context(&|_| Err("not a git repo".into()));
        assert!(ctx.remote_url.is_none());
        assert!(ctx.default_branch.is_none());
        assert!(ctx.repo_name.is_none());
    }

    #[test]
    fn detect_git_context_no_origin_head() {
        let ctx = detect_git_context(&|cmd| match cmd {
            "git remote get-url origin" => Ok("https://github.com/user/repo.git".into()),
            _ => Err("not set".into()),
        });
        assert!(ctx.remote_url.is_some());
        assert!(ctx.default_branch.is_none());
        assert_eq!(ctx.repo_name.as_deref(), Some("repo"));
    }

    // ── generate_config_toml ───────────────────────────────────────────

    #[test]
    fn generate_config_produces_valid_toml() {
        let answers = InitAnswers {
            host: "myhost".into(),
            session_prefix: "test-".into(),
            default_branch: "main".into(),
            base_path: "~/test".into(),
            worktree_base: "~/test-worktrees".into(),
            repo_url: "https://github.com/user/test.git".into(),
            repo_name: "test".into(),
            run_setup: false,
        };
        let toml_str = generate_config_toml(&answers);
        let cfg: Config = toml::from_str(&toml_str).expect("should parse as valid Config");
        assert_eq!(cfg.host, "myhost");
        assert_eq!(cfg.session_prefix, "test-");
        assert_eq!(cfg.base_path, "~/test");
        assert_eq!(cfg.worktree_base, "~/test-worktrees");
        assert_eq!(cfg.default_branch, "main");
    }

    // ── run_wizard ─────────────────────────────────────────────────────

    #[test]
    fn wizard_happy_path_with_git_context() {
        let mut prompter = MockPrompter::new(vec![
            "myserver", // SSH host
            "",         // session prefix (accept default)
            "",         // default branch (accept default)
            "y",        // run setup
        ]);
        let result = run_wizard(
            &mut prompter,
            &git_ctx_full(),
            false,
            false,
            &mock_ssh_test_ok,
        );
        let answers = result.expect("wizard should succeed");
        assert_eq!(answers.host, "myserver");
        assert_eq!(answers.session_prefix, "my-project-");
        assert_eq!(answers.default_branch, "main");
        assert_eq!(answers.base_path, "~/my-project");
        assert_eq!(answers.worktree_base, "~/my-project-worktrees");
        assert_eq!(answers.repo_url, "git@github.com:user/my-project.git");
        assert!(answers.run_setup);
    }

    #[test]
    fn wizard_happy_path_without_git_context() {
        let mut prompter = MockPrompter::new(vec![
            "https://github.com/user/cool-app.git", // repo URL
            "myserver",                             // SSH host
            "",                                     // session prefix (accept default)
            "",                                     // default branch (accept default "main")
            "n",                                    // skip setup
        ]);
        let result = run_wizard(
            &mut prompter,
            &git_ctx_empty(),
            false,
            false,
            &mock_ssh_test_ok,
        );
        let answers = result.expect("wizard should succeed");
        assert_eq!(answers.host, "myserver");
        assert_eq!(answers.session_prefix, "cool-app-");
        assert_eq!(answers.default_branch, "main");
        assert_eq!(answers.repo_name, "cool-app");
        assert!(!answers.run_setup);
    }

    #[test]
    fn wizard_aborts_when_config_exists_and_user_declines() {
        let mut prompter = MockPrompter::new(vec!["n"]);
        let result = run_wizard(
            &mut prompter,
            &git_ctx_full(),
            true,
            false,
            &mock_ssh_test_ok,
        );
        assert!(matches!(result, Err(SkulkError::InitAborted)));
    }

    #[test]
    fn wizard_reconfigures_when_config_exists_and_user_accepts() {
        let mut prompter = MockPrompter::new(vec![
            "y",       // reconfigure
            "newhost", // SSH host
            "custom-", // custom prefix
            "develop", // custom branch
            "n",       // skip setup
        ]);
        let result = run_wizard(
            &mut prompter,
            &git_ctx_full(),
            true,
            false,
            &mock_ssh_test_ok,
        );
        let answers = result.expect("wizard should succeed");
        assert_eq!(answers.host, "newhost");
        assert_eq!(answers.session_prefix, "custom-");
        assert_eq!(answers.default_branch, "develop");
    }

    #[test]
    fn wizard_uses_defaults_for_prefix_and_branch() {
        let mut prompter = MockPrompter::new(vec![
            "myserver", // host
            "",         // prefix default
            "",         // branch default
            "n",        // skip setup
        ]);
        let result = run_wizard(
            &mut prompter,
            &git_ctx_full(),
            false,
            false,
            &mock_ssh_test_ok,
        );
        let answers = result.expect("wizard should succeed");
        assert_eq!(answers.session_prefix, "my-project-");
        assert_eq!(answers.default_branch, "main");
    }

    #[test]
    fn wizard_ssh_test_retry_then_succeed() {
        let call_count = std::cell::Cell::new(0);
        let test_ssh = |_host: &str| -> Result<(), SkulkError> {
            let count = call_count.get();
            call_count.set(count + 1);
            if count == 0 {
                Err(SkulkError::Diagnostic {
                    message: "Connection refused.".into(),
                    suggestion: "Check host.".into(),
                })
            } else {
                Ok(())
            }
        };
        let mut prompter = MockPrompter::new(vec![
            "myserver", // host
            "y",        // retry SSH
            "",         // prefix default
            "",         // branch default
            "n",        // skip setup
        ]);
        let result = run_wizard(&mut prompter, &git_ctx_full(), false, false, &test_ssh);
        assert!(result.is_ok());
    }

    #[test]
    fn wizard_ssh_test_retry_then_abort() {
        let mut prompter = MockPrompter::new(vec![
            "myserver", // host
            "n",        // don't retry
        ]);
        let result = run_wizard(
            &mut prompter,
            &git_ctx_full(),
            false,
            false,
            &mock_ssh_test_fail,
        );
        assert!(matches!(result, Err(SkulkError::InitAborted)));
    }

    #[test]
    fn wizard_rejects_unsafe_manual_repo_name() {
        // Git context has URL but no parseable repo name
        let ctx = GitContext {
            remote_url: Some("not-a-url".into()),
            default_branch: Some("main".into()),
            repo_name: None,
        };
        let mut prompter = MockPrompter::new(vec![
            "bad name",  // repo name with space (rejected)
            "good-name", // repo name (accepted)
            "myserver",  // SSH host
            "",          // prefix default
            "",          // branch default
            "n",         // skip setup
        ]);
        let result = run_wizard(&mut prompter, &ctx, false, false, &mock_ssh_test_ok);
        let answers = result.expect("wizard should succeed after retry");
        assert_eq!(answers.repo_name, "good-name");
    }

    #[test]
    fn wizard_validates_auto_detected_repo_name() {
        // Git context with a repo_name that somehow contains unsafe chars
        let bad_ctx = GitContext {
            remote_url: Some("https://example.com/repo.git".into()),
            default_branch: Some("main".into()),
            repo_name: Some("bad name".into()),
        };
        let mut prompter = MockPrompter::new(vec![
            "myserver", // SSH host (never reached)
        ]);
        let result = run_wizard(&mut prompter, &bad_ctx, false, false, &mock_ssh_test_ok);
        assert!(result.is_err());
    }

    // ── setup commands ─────────────────────────────────────────────────

    #[test]
    fn setup_check_command_checks_all_components() {
        let answers = InitAnswers {
            host: "h".into(),
            session_prefix: "s-".into(),
            default_branch: "main".into(),
            base_path: "~/project".into(),
            worktree_base: "~/project-wt".into(),
            repo_url: "u".into(),
            repo_name: "project".into(),
            run_setup: true,
        };
        let cmd = setup_check_command(&answers);
        assert!(cmd.contains("tmux"));
        assert!(cmd.contains("git"));
        assert!(cmd.contains("claude"));
        assert!(cmd.contains("~/project/.git"));
        assert!(cmd.contains("~/project-wt"));
    }

    #[test]
    fn parse_setup_status_all_installed() {
        let output =
            "tmux:installed\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists\n";
        let status = parse_setup_status(output);
        assert_eq!(status.get("tmux").unwrap(), "installed");
        assert_eq!(status.get("claude").unwrap(), "installed");
        assert_eq!(status.get("repo").unwrap(), "cloned");
    }

    #[test]
    fn parse_setup_status_empty() {
        let status = parse_setup_status("");
        assert!(status.is_empty());
    }

    #[test]
    fn setup_install_tmux() {
        let cmd = setup_install_command("tmux");
        assert!(cmd.contains("apt-get") && cmd.contains("tmux"));
    }

    #[test]
    fn setup_install_git() {
        let cmd = setup_install_command("git");
        assert!(cmd.contains("apt-get") && cmd.contains("git"));
    }

    #[test]
    fn setup_install_claude() {
        let cmd = setup_install_command("claude");
        assert!(cmd.contains("curl") && cmd.contains("install"));
    }

    #[test]
    fn setup_clone_command_escapes_url() {
        let cmd = setup_clone_command("https://github.com/user/repo.git", "~/repo");
        assert!(cmd.contains("git clone 'https://github.com/user/repo.git'"));
        assert!(cmd.contains("~/repo"));
    }

    #[test]
    fn setup_create_worktree_dir_command_generates() {
        let cmd = setup_create_worktree_dir_command("~/my-worktrees");
        assert!(cmd.contains("mkdir -p ~/my-worktrees"));
    }

    // ── run_remote_setup ───────────────────────────────────────────────

    fn test_answers() -> InitAnswers {
        InitAnswers {
            host: "testhost".into(),
            session_prefix: "test-".into(),
            default_branch: "main".into(),
            base_path: "~/test".into(),
            worktree_base: "~/test-worktrees".into(),
            repo_url: "https://example.com/repo.git".into(),
            repo_name: "test".into(),
            run_setup: true,
        }
    }

    #[test]
    fn remote_setup_all_installed() {
        let ssh =
            MockSsh::new(vec![
            Ok(String::new()), // apt-get check
            Ok("tmux:installed\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists"
                .into()),
        ]);
        assert!(run_remote_setup(&ssh, &test_answers(), false).is_ok());
    }

    #[test]
    fn remote_setup_installs_missing_tools() {
        let ssh =
            MockSsh::new(vec![
            Ok(String::new()), // apt-get check
            Ok("tmux:missing\ngit:installed\nclaude:missing\nrepo:missing\nworktree-dir:missing"
                .into()),
            Ok(String::new()), // tmux install
            Ok(String::new()), // claude install
            Ok(String::new()), // clone
            Ok(String::new()), // mkdir
        ]);
        assert!(run_remote_setup(&ssh, &test_answers(), false).is_ok());
    }

    #[test]
    fn remote_setup_no_apt_get_returns_error() {
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("command not found".into()))]);
        let result = run_remote_setup(&ssh, &test_answers(), false);
        assert!(result.is_err());
    }

    #[test]
    fn remote_setup_tool_install_fails_continues() {
        let ssh =
            MockSsh::new(vec![
            Ok(String::new()), // apt-get check
            Ok("tmux:missing\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists"
                .into()),
            Err(SkulkError::SshFailed("install failed".into())), // tmux install fails
        ]);
        // Should succeed (tool install failures are non-fatal)
        assert!(run_remote_setup(&ssh, &test_answers(), false).is_ok());
    }

    #[test]
    fn remote_setup_repo_already_cloned() {
        let ssh =
            MockSsh::new(vec![
            Ok(String::new()), // apt-get check
            Ok("tmux:installed\ngit:installed\nclaude:installed\nrepo:cloned\nworktree-dir:exists"
                .into()),
        ]);
        assert!(run_remote_setup(&ssh, &test_answers(), false).is_ok());
    }

    // ── display ────────────────────────────────────────────────────────

    #[test]
    fn welcome_banner_contains_wolf_when_color() {
        let banner = welcome_banner(true);
        assert!(banner.contains('\u{1f43a}')); // 🐺
    }

    #[test]
    fn welcome_banner_no_color_has_no_ansi() {
        let banner = welcome_banner(false);
        assert!(!banner.contains("\x1b["));
    }

    #[test]
    fn success_message_includes_skulk_new() {
        let answers = test_answers();
        let msg = success_message(&answers, false);
        assert!(msg.contains("skulk new"));
    }

    #[test]
    fn success_message_contains_party_when_color() {
        let answers = test_answers();
        let msg = success_message(&answers, true);
        assert!(msg.contains('\u{1f389}')); // 🎉
    }
}
