//! `skulk doctor` — health check for the runtime environment.
//!
//! Runs a sequence of pass/fail/warn checks against the local config and the
//! remote host. All remote checks share a single SSH roundtrip so the command
//! is fast even on slow links.

use crate::config::{self, Config};
use crate::display::{checkmark, crossmark, dim, use_color, warnmark};
use crate::error::SkulkError;
use crate::ssh::Ssh;

/// Local skulk binary version, baked in at compile time.
const LOCAL_SKULK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Width of the label column (e.g. `"Worktree dir:"`). Wide enough to fit the
/// longest label plus its trailing colon and one space of padding.
const LABEL_WIDTH: usize = 14;

/// Minimum width of the value column. Values that overflow push the status
/// marker rightward instead of being truncated.
const VALUE_WIDTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug)]
struct CheckRow {
    label: &'static str,
    value: String,
    status: CheckStatus,
    /// Indented note printed below the row when present. Used for failure
    /// suggestions; multi-line notes are indented per-line.
    note: Option<String>,
}

// ── Probe ──────────────────────────────────────────────────────────────────

/// Build the single SSH probe that gathers tool versions and path existence.
///
/// Output is line-based `key:value` pairs the parser walks once. Tool checks
/// emit `<tool>:installed:<version>` or `<tool>:missing`; gh has an extra
/// `gh-auth:yes|no|na` line; path checks emit `base:exists|missing` and
/// `worktree:exists|missing`.
pub(crate) fn probe_command(cfg: &Config) -> String {
    let base = &cfg.base_path;
    let wt = &cfg.worktree_base;
    format!(
        "if command -v tmux >/dev/null 2>&1; then \
            v=$(tmux -V 2>&1); echo \"tmux:installed:$v\"; \
         else echo \"tmux:missing\"; fi; \
         if command -v claude >/dev/null 2>&1; then \
            v=$(claude --version 2>&1 | head -n1); echo \"claude:installed:$v\"; \
         elif [ -x ~/.local/bin/claude ]; then \
            v=$(~/.local/bin/claude --version 2>&1 | head -n1); echo \"claude:installed:$v\"; \
         else echo \"claude:missing\"; fi; \
         if command -v gh >/dev/null 2>&1; then \
            v=$(gh --version 2>&1 | head -n1); echo \"gh:installed:$v\"; \
            if gh auth status >/dev/null 2>&1; then \
                echo \"gh-auth:yes\"; \
            else echo \"gh-auth:no\"; fi; \
         else echo \"gh:missing\"; echo \"gh-auth:na\"; fi; \
         if [ -d {base}/.git ]; then echo \"base:exists\"; else echo \"base:missing\"; fi; \
         if [ -d {wt} ]; then echo \"worktree:exists\"; else echo \"worktree:missing\"; fi"
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ProbeResults {
    /// `Some(version_string)` if installed, `None` if missing.
    tmux: Option<String>,
    claude: Option<String>,
    gh: Option<String>,
    gh_authenticated: bool,
    base_exists: bool,
    worktree_exists: bool,
}

fn parse_probe_output(output: &str) -> ProbeResults {
    let mut r = ProbeResults::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("tmux:") {
            r.tmux = parse_installed(rest);
        } else if let Some(rest) = line.strip_prefix("claude:") {
            r.claude = parse_installed(rest);
        } else if let Some(rest) = line.strip_prefix("gh:") {
            r.gh = parse_installed(rest);
        } else if line == "gh-auth:yes" {
            r.gh_authenticated = true;
        } else if line == "base:exists" {
            r.base_exists = true;
        } else if line == "worktree:exists" {
            r.worktree_exists = true;
        }
    }
    r
}

fn parse_installed(rest: &str) -> Option<String> {
    rest.strip_prefix("installed:")
        .map(|v| clean_version(v.trim()))
}

/// Strip well-known tool name prefixes from a version string so the displayed
/// value is the version itself (`3.3a`) rather than the raw command output
/// (`tmux 3.3a`).
fn clean_version(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("tmux ") {
        rest.trim().to_string()
    } else if let Some(rest) = trimmed.strip_prefix("gh version ") {
        rest.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

// ── Row construction ───────────────────────────────────────────────────────

fn config_row(cfg: &Config) -> CheckRow {
    let display = cfg.root_dir.as_ref().map_or_else(
        || format!("{}/{}", config::CONFIG_DIR, config::CONFIG_FILENAME),
        |root| {
            format!(
                "{}/{}/{}",
                root.display(),
                config::CONFIG_DIR,
                config::CONFIG_FILENAME
            )
        },
    );
    CheckRow {
        label: "Config",
        value: display,
        status: CheckStatus::Ok,
        note: None,
    }
}

fn skulk_row() -> CheckRow {
    CheckRow {
        label: "skulk",
        value: LOCAL_SKULK_VERSION.to_string(),
        status: CheckStatus::Ok,
        note: None,
    }
}

fn ssh_ok_row(cfg: &Config) -> CheckRow {
    CheckRow {
        label: "SSH",
        value: cfg.host.clone(),
        status: CheckStatus::Ok,
        note: None,
    }
}

fn ssh_fail_row(cfg: &Config, err: &SkulkError) -> CheckRow {
    let note = match err {
        SkulkError::Diagnostic {
            message,
            suggestion,
        } => format!("{message}\nSuggestion: {suggestion}"),
        other => other.to_string(),
    };
    CheckRow {
        label: "SSH",
        value: cfg.host.clone(),
        status: CheckStatus::Fail,
        note: Some(note),
    }
}

fn skipped_remote_rows(cfg: &Config) -> Vec<CheckRow> {
    vec![
        skipped_row("tmux", "—"),
        skipped_row("claude", "—"),
        skipped_row("gh", "—"),
        skipped_row("Base clone", &cfg.base_path),
        skipped_row("Worktree dir", &cfg.worktree_base),
    ]
}

fn skipped_row(label: &'static str, value: &str) -> CheckRow {
    CheckRow {
        label,
        value: value.to_string(),
        status: CheckStatus::Skipped,
        note: None,
    }
}

fn remote_rows(probe: &ProbeResults, cfg: &Config) -> Vec<CheckRow> {
    vec![
        tmux_row(probe.tmux.as_deref(), &cfg.host),
        claude_row(probe.claude.as_deref(), &cfg.host),
        gh_row(probe.gh.as_deref(), probe.gh_authenticated, &cfg.host),
        base_clone_row(probe.base_exists, &cfg.base_path, &cfg.host),
        worktree_dir_row(probe.worktree_exists, &cfg.worktree_base, &cfg.host),
    ]
}

fn tmux_row(version: Option<&str>, host: &str) -> CheckRow {
    match version {
        Some(v) => CheckRow {
            label: "tmux",
            value: v.to_string(),
            status: CheckStatus::Ok,
            note: None,
        },
        None => CheckRow {
            label: "tmux",
            value: "missing".into(),
            status: CheckStatus::Fail,
            note: Some(format!(
                "tmux is not installed on {host}.\n\
                 Install with: ssh {host} 'sudo apt-get install -y tmux'"
            )),
        },
    }
}

fn claude_row(version: Option<&str>, host: &str) -> CheckRow {
    match version {
        Some(v) => CheckRow {
            label: "claude",
            value: v.to_string(),
            status: CheckStatus::Ok,
            note: None,
        },
        None => CheckRow {
            label: "claude",
            value: "missing".into(),
            status: CheckStatus::Fail,
            note: Some(format!(
                "Claude Code is not installed on {host}.\n\
                 Install with: ssh {host} 'curl -fsSL https://claude.ai/install.sh | sh'"
            )),
        },
    }
}

fn gh_row(version: Option<&str>, authenticated: bool, host: &str) -> CheckRow {
    match (version, authenticated) {
        (Some(v), true) => CheckRow {
            label: "gh",
            value: format!("authenticated ({v})"),
            status: CheckStatus::Ok,
            note: None,
        },
        (Some(v), false) => CheckRow {
            label: "gh",
            value: format!("{v} (not authenticated)"),
            status: CheckStatus::Warn,
            note: Some(format!(
                "gh is installed but not authenticated. \
                 Required for `skulk new --github` and `skulk ship`.\n\
                 Authenticate with: ssh -t {host} gh auth login"
            )),
        },
        (None, _) => CheckRow {
            label: "gh",
            value: "missing".into(),
            status: CheckStatus::Warn,
            note: Some(format!(
                "gh is not installed on {host}. \
                 Required for `skulk new --github` and `skulk ship`.\n\
                 Install with: ssh {host} 'sudo apt-get install -y gh'"
            )),
        },
    }
}

fn base_clone_row(exists: bool, base_path: &str, host: &str) -> CheckRow {
    if exists {
        CheckRow {
            label: "Base clone",
            value: base_path.to_string(),
            status: CheckStatus::Ok,
            note: None,
        }
    } else {
        CheckRow {
            label: "Base clone",
            value: base_path.to_string(),
            status: CheckStatus::Fail,
            note: Some(format!(
                "Base clone not found at {base_path} on {host}.\n\
                 Clone with: ssh {host} 'git clone <your-repo-url> {base_path}'"
            )),
        }
    }
}

fn worktree_dir_row(exists: bool, worktree_base: &str, host: &str) -> CheckRow {
    if exists {
        CheckRow {
            label: "Worktree dir",
            value: worktree_base.to_string(),
            status: CheckStatus::Ok,
            note: None,
        }
    } else {
        CheckRow {
            label: "Worktree dir",
            value: worktree_base.to_string(),
            status: CheckStatus::Fail,
            note: Some(format!(
                "Worktree directory does not exist on {host}.\n\
                 Create with: ssh {host} 'mkdir -p {worktree_base}'"
            )),
        }
    }
}

// ── Rendering ──────────────────────────────────────────────────────────────

fn render_status(status: CheckStatus, color: bool) -> String {
    match status {
        CheckStatus::Ok => checkmark(color).to_string(),
        CheckStatus::Warn => warnmark(color).to_string(),
        CheckStatus::Fail => crossmark(color).to_string(),
        CheckStatus::Skipped => dim("[skip]", color),
    }
}

fn render_row(row: &CheckRow, color: bool) -> String {
    let label = format!("{}:", row.label);
    let line = format!(
        "{label:<LABEL_WIDTH$}{value:<VALUE_WIDTH$}{status}",
        value = row.value,
        status = render_status(row.status, color),
    );
    if let Some(note) = &row.note {
        let prefix = " ".repeat(LABEL_WIDTH);
        let indented: Vec<String> = note.lines().map(|l| format!("{prefix}{l}")).collect();
        format!("{line}\n{}", indented.join("\n"))
    } else {
        line
    }
}

// ── Entry point ────────────────────────────────────────────────────────────

pub(crate) fn cmd_doctor(ssh: &impl Ssh, cfg: &Config) -> Result<(), SkulkError> {
    let color = use_color();
    let mut rows: Vec<CheckRow> = Vec::with_capacity(8);

    rows.push(config_row(cfg));
    rows.push(skulk_row());

    match ssh.run(&probe_command(cfg)) {
        Ok(output) => {
            rows.push(ssh_ok_row(cfg));
            rows.extend(remote_rows(&parse_probe_output(&output), cfg));
        }
        Err(e) => {
            rows.push(ssh_fail_row(cfg, &e));
            rows.extend(skipped_remote_rows(cfg));
        }
    }

    for row in &rows {
        println!("{}", render_row(row, color));
    }

    let fail_count = rows
        .iter()
        .filter(|r| r.status == CheckStatus::Fail)
        .count();
    if fail_count > 0 {
        let plural = if fail_count == 1 { "" } else { "s" };
        return Err(SkulkError::Validation(format!(
            "{fail_count} check{plural} failed."
        )));
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, assert_err, test_config};

    fn probe_output_all_ok() -> String {
        "tmux:installed:tmux 3.3a\n\
         claude:installed:1.2.0\n\
         gh:installed:gh version 2.40.1 (2023-12-13)\n\
         gh-auth:yes\n\
         base:exists\n\
         worktree:exists\n"
            .to_string()
    }

    // ── probe_command ──────────────────────────────────────────────────

    #[test]
    fn probe_command_includes_all_checks() {
        let cfg = test_config();
        let cmd = probe_command(&cfg);
        assert!(cmd.contains("tmux -V"));
        assert!(cmd.contains("claude --version"));
        assert!(cmd.contains("gh --version"));
        assert!(cmd.contains("gh auth status"));
        assert!(cmd.contains("~/test-project/.git"));
        assert!(cmd.contains("~/test-project-worktrees"));
    }

    #[test]
    fn probe_command_uses_configured_paths() {
        let mut cfg = test_config();
        cfg.base_path = "~/custom-repo".into();
        cfg.worktree_base = "~/custom-wt".into();
        let cmd = probe_command(&cfg);
        assert!(cmd.contains("~/custom-repo/.git"));
        assert!(cmd.contains("~/custom-wt"));
    }

    // ── parse_probe_output ─────────────────────────────────────────────

    #[test]
    fn parse_probe_all_installed_authenticated() {
        let r = parse_probe_output(&probe_output_all_ok());
        assert_eq!(r.tmux.as_deref(), Some("3.3a"));
        assert_eq!(r.claude.as_deref(), Some("1.2.0"));
        assert_eq!(r.gh.as_deref(), Some("2.40.1 (2023-12-13)"));
        assert!(r.gh_authenticated);
        assert!(r.base_exists);
        assert!(r.worktree_exists);
    }

    #[test]
    fn parse_probe_all_missing() {
        let output = "tmux:missing\nclaude:missing\ngh:missing\ngh-auth:na\n\
                      base:missing\nworktree:missing\n";
        let r = parse_probe_output(output);
        assert!(r.tmux.is_none());
        assert!(r.claude.is_none());
        assert!(r.gh.is_none());
        assert!(!r.gh_authenticated);
        assert!(!r.base_exists);
        assert!(!r.worktree_exists);
    }

    #[test]
    fn parse_probe_gh_installed_but_unauthenticated() {
        let output = "tmux:installed:tmux 3.3a\nclaude:installed:1.2.0\n\
                      gh:installed:gh version 2.40.1\ngh-auth:no\n\
                      base:exists\nworktree:exists\n";
        let r = parse_probe_output(output);
        assert!(r.gh.is_some());
        assert!(!r.gh_authenticated);
    }

    #[test]
    fn parse_probe_ignores_unknown_lines() {
        let output = "garbage line\ntmux:installed:tmux 3.3a\nrandom\n";
        let r = parse_probe_output(output);
        assert_eq!(r.tmux.as_deref(), Some("3.3a"));
    }

    #[test]
    fn clean_version_strips_tmux_prefix() {
        assert_eq!(clean_version("tmux 3.3a"), "3.3a");
    }

    #[test]
    fn clean_version_strips_gh_prefix() {
        assert_eq!(
            clean_version("gh version 2.40.1 (2023-12-13)"),
            "2.40.1 (2023-12-13)"
        );
    }

    #[test]
    fn clean_version_passthrough_for_claude() {
        assert_eq!(clean_version("1.2.0 (Claude Code)"), "1.2.0 (Claude Code)");
    }

    // ── remote_rows ────────────────────────────────────────────────────

    #[test]
    fn remote_rows_all_ok() {
        let cfg = test_config();
        let probe = parse_probe_output(&probe_output_all_ok());
        let rows = remote_rows(&probe, &cfg);
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|r| r.status == CheckStatus::Ok));
    }

    #[test]
    fn remote_rows_missing_tmux_fails() {
        let cfg = test_config();
        let probe = ProbeResults {
            tmux: None,
            claude: Some("1.2.0".into()),
            gh: Some("2.40.1".into()),
            gh_authenticated: true,
            base_exists: true,
            worktree_exists: true,
        };
        let rows = remote_rows(&probe, &cfg);
        let tmux_row = &rows[0];
        assert_eq!(tmux_row.label, "tmux");
        assert_eq!(tmux_row.status, CheckStatus::Fail);
        assert!(tmux_row.note.as_ref().unwrap().contains("apt-get install"));
    }

    #[test]
    fn remote_rows_missing_gh_warns_not_fails() {
        let cfg = test_config();
        let probe = ProbeResults {
            tmux: Some("3.3a".into()),
            claude: Some("1.2.0".into()),
            gh: None,
            gh_authenticated: false,
            base_exists: true,
            worktree_exists: true,
        };
        let rows = remote_rows(&probe, &cfg);
        let gh_row = rows.iter().find(|r| r.label == "gh").unwrap();
        assert_eq!(gh_row.status, CheckStatus::Warn);
    }

    #[test]
    fn remote_rows_unauthenticated_gh_warns() {
        let cfg = test_config();
        let probe = ProbeResults {
            tmux: Some("3.3a".into()),
            claude: Some("1.2.0".into()),
            gh: Some("2.40.1".into()),
            gh_authenticated: false,
            base_exists: true,
            worktree_exists: true,
        };
        let rows = remote_rows(&probe, &cfg);
        let gh_row = rows.iter().find(|r| r.label == "gh").unwrap();
        assert_eq!(gh_row.status, CheckStatus::Warn);
        assert!(gh_row.note.as_ref().unwrap().contains("gh auth login"));
    }

    #[test]
    fn remote_rows_missing_base_clone_fails() {
        let cfg = test_config();
        let probe = ProbeResults {
            tmux: Some("3.3a".into()),
            claude: Some("1.2.0".into()),
            gh: Some("2.40.1".into()),
            gh_authenticated: true,
            base_exists: false,
            worktree_exists: true,
        };
        let rows = remote_rows(&probe, &cfg);
        let row = rows.iter().find(|r| r.label == "Base clone").unwrap();
        assert_eq!(row.status, CheckStatus::Fail);
        assert!(row.note.as_ref().unwrap().contains("git clone"));
    }

    #[test]
    fn remote_rows_missing_worktree_dir_fails() {
        let cfg = test_config();
        let probe = ProbeResults {
            tmux: Some("3.3a".into()),
            claude: Some("1.2.0".into()),
            gh: Some("2.40.1".into()),
            gh_authenticated: true,
            base_exists: true,
            worktree_exists: false,
        };
        let rows = remote_rows(&probe, &cfg);
        let row = rows.iter().find(|r| r.label == "Worktree dir").unwrap();
        assert_eq!(row.status, CheckStatus::Fail);
        assert!(row.note.as_ref().unwrap().contains("mkdir -p"));
    }

    // ── render_row ─────────────────────────────────────────────────────

    #[test]
    fn render_row_appends_indented_note() {
        let row = CheckRow {
            label: "SSH",
            value: "myhost".into(),
            status: CheckStatus::Fail,
            note: Some("line one\nline two".into()),
        };
        let rendered = render_row(&row, false);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("SSH:"));
        assert!(lines[0].contains("myhost"));
        assert!(lines[0].contains("[FAIL]"));
        let prefix = " ".repeat(LABEL_WIDTH);
        assert_eq!(lines[1], format!("{prefix}line one"));
        assert_eq!(lines[2], format!("{prefix}line two"));
    }

    #[test]
    fn render_row_no_note_is_single_line() {
        let row = CheckRow {
            label: "Config",
            value: ".skulk/config.toml".into(),
            status: CheckStatus::Ok,
            note: None,
        };
        let rendered = render_row(&row, false);
        assert_eq!(rendered.lines().count(), 1);
        assert!(rendered.contains("[ok]"));
    }

    // ── cmd_doctor ─────────────────────────────────────────────────────

    #[test]
    fn cmd_doctor_all_ok_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(probe_output_all_ok())]);
        assert!(cmd_doctor(&ssh, &cfg).is_ok());
    }

    #[test]
    fn cmd_doctor_returns_error_when_check_fails() {
        let cfg = test_config();
        let probe = "tmux:missing\nclaude:installed:1.2.0\n\
                     gh:installed:gh version 2.40.1\ngh-auth:yes\n\
                     base:exists\nworktree:exists\n";
        let ssh = MockSsh::new(vec![Ok(probe.to_string())]);
        let result = cmd_doctor(&ssh, &cfg);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("1 check failed"));
        });
    }

    #[test]
    fn cmd_doctor_pluralizes_failure_count() {
        let cfg = test_config();
        let probe = "tmux:missing\nclaude:missing\n\
                     gh:installed:gh version 2.40.1\ngh-auth:yes\n\
                     base:exists\nworktree:exists\n";
        let ssh = MockSsh::new(vec![Ok(probe.to_string())]);
        let result = cmd_doctor(&ssh, &cfg);
        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("2 checks failed"));
        });
    }

    #[test]
    fn cmd_doctor_warn_only_does_not_fail() {
        let cfg = test_config();
        // gh missing → warn, everything else ok
        let probe = "tmux:installed:tmux 3.3a\nclaude:installed:1.2.0\n\
                     gh:missing\ngh-auth:na\n\
                     base:exists\nworktree:exists\n";
        let ssh = MockSsh::new(vec![Ok(probe.to_string())]);
        assert!(cmd_doctor(&ssh, &cfg).is_ok());
    }

    #[test]
    fn cmd_doctor_ssh_failure_marks_remote_skipped_and_fails() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "SSH connection refused by testhost.".into(),
            suggestion: "Ensure SSH is running on testhost.".into(),
        })]);
        let result = cmd_doctor(&ssh, &cfg);
        assert_err!(result, SkulkError::Validation(msg) => {
            // Only SSH itself counts as a failure when it fails — remote checks are skipped.
            assert!(msg.contains("1 check failed"));
        });
    }

    #[test]
    fn cmd_doctor_runs_single_ssh_roundtrip() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(probe_output_all_ok())]);
        let _ = cmd_doctor(&ssh, &cfg);
        // Single roundtrip is a hard requirement for snappiness on slow links.
        assert_eq!(ssh.calls().len(), 1);
    }

    #[test]
    fn ssh_fail_row_includes_diagnostic_suggestion() {
        let cfg = test_config();
        let err = SkulkError::Diagnostic {
            message: "Cannot resolve hostname 'testhost'.".into(),
            suggestion: "Check your DNS.".into(),
        };
        let row = ssh_fail_row(&cfg, &err);
        assert_eq!(row.status, CheckStatus::Fail);
        let note = row.note.as_ref().unwrap();
        assert!(note.contains("resolve hostname"));
        assert!(note.contains("Check your DNS"));
    }
}
