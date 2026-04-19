use std::fmt::Write;
use std::path::Path;

use serde::Deserialize;

use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;

// ── Types ──────────────────────────────────────────────────────────────────

/// Parsed subset of `gh issue view <id> --json title,body,comments` output.
#[derive(Debug, Deserialize)]
pub(crate) struct GhIssue {
    pub title: String,
    pub body: String,
    pub comments: Vec<GhComment>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GhComment {
    pub author: GhAuthor,
    pub body: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GhAuthor {
    pub login: String,
}

// ── Validation ─────────────────────────────────────────────────────────────

/// Validate a GitHub issue ID: must be a non-empty sequence of ASCII digits.
///
/// Cross-repo syntax like `owner/repo#123` is intentionally unsupported for now.
pub(crate) fn validate_issue_id(id: &str) -> Result<(), SkulkError> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(SkulkError::Validation(format!(
            "Invalid GitHub issue ID '{id}'. Must be a positive integer (e.g. 42)."
        )));
    }
    Ok(())
}

// ── Remote gh detection ────────────────────────────────────────────────────

/// Build the SSH command that checks whether `gh` is installed and authenticated
/// on the remote, printing a marker to stdout for each outcome.
///
/// Exits 0 always so the ssh layer doesn't swallow our marker through generic
/// "command not found" / "authentication" classification in `classify_ssh_error`.
pub(crate) fn gh_availability_command() -> &'static str {
    "if ! command -v gh >/dev/null 2>&1; then \
         echo 'SKULK_GH_MISSING'; \
         exit 0; \
     fi; \
     if ! gh auth status >/dev/null 2>&1; then \
         echo 'SKULK_GH_UNAUTHENTICATED'; \
         exit 0; \
     fi; \
     echo 'SKULK_GH_OK'"
}

/// Check gh availability on the remote, returning a diagnostic error if missing or unauthenticated.
pub(crate) fn check_gh_available(ssh: &impl Ssh, cfg: &Config) -> Result<(), SkulkError> {
    let output = ssh.run(gh_availability_command())?;
    if output.contains("SKULK_GH_MISSING") {
        Err(SkulkError::Diagnostic {
            message: format!("`gh` is not installed on {}.", cfg.host),
            suggestion: format!(
                "Re-run `skulk init` to install gh, or install manually: ssh {} 'sudo apt-get install gh'",
                cfg.host
            ),
        })
    } else if output.contains("SKULK_GH_UNAUTHENTICATED") {
        Err(SkulkError::Diagnostic {
            message: format!("`gh` is not authenticated on {}.", cfg.host),
            suggestion: format!(
                "Authenticate on the remote: ssh -t {} gh auth login",
                cfg.host
            ),
        })
    } else {
        Ok(())
    }
}

// ── Issue fetch ────────────────────────────────────────────────────────────

/// Build the SSH command that fetches an issue as JSON from the base repo on the remote.
///
/// Private to the module: callers must go through `load_github_prompt`, which validates
/// `issue_id` first via `validate_issue_id`. Keeping this function private prevents a
/// future caller from accidentally interpolating an unvalidated id into a shell string.
fn gh_issue_fetch_command(issue_id: &str, cfg: &Config) -> String {
    format!(
        "cd {} && gh issue view {} --json title,body,comments",
        cfg.base_path, issue_id
    )
}

/// Fetch a GitHub issue via `gh` on the remote and return the raw JSON.
///
/// Translates "Could not resolve to an Issue" into a clean `NotFound` error.
///
/// Private to the module for the same reason as `gh_issue_fetch_command`: this is
/// the only path that interpolates `issue_id` into a remote shell command.
fn fetch_github_issue_raw(
    ssh: &impl Ssh,
    issue_id: &str,
    cfg: &Config,
) -> Result<String, SkulkError> {
    match ssh.run(&gh_issue_fetch_command(issue_id, cfg)) {
        Ok(output) => Ok(output),
        Err(SkulkError::SshFailed(stderr))
            if stderr
                .to_lowercase()
                .contains("could not resolve to an issue") =>
        {
            Err(SkulkError::NotFound(format!(
                "GitHub issue #{issue_id} not found in the repository on {}.",
                cfg.host
            )))
        }
        Err(e) => Err(e),
    }
}

/// Parse raw `gh issue view --json title,body,comments` output into a `GhIssue`.
pub(crate) fn parse_gh_issue(raw: &str) -> Result<GhIssue, SkulkError> {
    serde_json::from_str::<GhIssue>(raw)
        .map_err(|e| SkulkError::SshFailed(format!("Failed to parse gh output as JSON: {e}")))
}

// ── Prompt wrapping ────────────────────────────────────────────────────────

/// Wrap a raw file's contents into the task prompt sent to the agent.
pub(crate) fn wrap_file_prompt(branch: &str, contents: &str) -> String {
    format!(
        "You've been assigned a task. Read it carefully, then ask me clarifying questions one at a time before you start implementing.\n\n\
         You're working in a dedicated git worktree on branch `{branch}` — feel free to commit freely; the branch is isolated.\n\n\
         ---\n\
         {contents}\n\
         ---"
    )
}

/// Wrap a parsed GitHub issue into the task prompt sent to the agent.
pub(crate) fn wrap_github_prompt(issue_id: &str, branch: &str, issue: &GhIssue) -> String {
    let mut out = String::new();
    // Writes into a String never fail; ignore the Result.
    let _ = write!(
        out,
        "You've been assigned GitHub issue #{issue_id}. The full issue and all comments are below. Read them carefully, then ask me clarifying questions one at a time before you start implementing.\n\n"
    );
    let _ = write!(
        out,
        "You're working in a dedicated git worktree on branch `{branch}` — feel free to commit freely. You have `gh` available if you need to interact with the issue further.\n\n"
    );
    let _ = writeln!(out, "--- Issue #{issue_id}: {} ---", issue.title);
    out.push_str(&issue.body);
    let _ = write!(out, "\n\n--- Comments ({}) ---\n", issue.comments.len());
    for c in &issue.comments {
        let _ = write!(
            out,
            "{} ({}):\n{}\n\n",
            c.author.login, c.created_at, c.body
        );
    }
    // Trim trailing whitespace so callers don't get a stray blank line.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

// ── Loaders (full pipelines) ───────────────────────────────────────────────

/// Load a local text file and wrap it into an agent prompt.
pub(crate) fn load_file_prompt(path: &Path, branch: &str) -> Result<String, SkulkError> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        SkulkError::Validation(format!(
            "Failed to read prompt file {}: {e}",
            path.display()
        ))
    })?;
    if contents.trim().is_empty() {
        return Err(SkulkError::Validation(format!(
            "Prompt file {} is empty.",
            path.display()
        )));
    }
    Ok(wrap_file_prompt(branch, contents.trim_end_matches('\n')))
}

/// Fetch a GitHub issue from the remote and wrap it into an agent prompt.
pub(crate) fn load_github_prompt(
    ssh: &impl Ssh,
    issue_id: &str,
    branch: &str,
    cfg: &Config,
) -> Result<String, SkulkError> {
    validate_issue_id(issue_id)?;
    check_gh_available(ssh, cfg)?;
    let raw = fetch_github_issue_raw(ssh, issue_id, cfg)?;
    let issue = parse_gh_issue(&raw)?;
    Ok(wrap_github_prompt(issue_id, branch, &issue))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, test_config};

    // ── validate_issue_id ──────────────────────────────────────────────

    #[test]
    fn validate_issue_id_accepts_digits() {
        assert!(validate_issue_id("42").is_ok());
        assert!(validate_issue_id("1").is_ok());
        assert!(validate_issue_id("99999").is_ok());
    }

    #[test]
    fn validate_issue_id_rejects_empty() {
        let err = validate_issue_id("").unwrap_err();
        assert!(matches!(err, SkulkError::Validation(_)));
    }

    #[test]
    fn validate_issue_id_rejects_non_digits() {
        assert!(validate_issue_id("abc").is_err());
        assert!(validate_issue_id("42a").is_err());
        assert!(validate_issue_id("-1").is_err());
        assert!(validate_issue_id("1.5").is_err());
    }

    #[test]
    fn validate_issue_id_rejects_cross_repo_syntax() {
        // Cross-repo syntax is explicitly out of scope for now.
        assert!(validate_issue_id("owner/repo#123").is_err());
        assert!(validate_issue_id("#123").is_err());
    }

    // ── parse_gh_issue ─────────────────────────────────────────────────

    #[test]
    fn parse_gh_issue_no_comments() {
        let raw = r#"{"title":"Bug","body":"Broken","comments":[]}"#;
        let issue = parse_gh_issue(raw).unwrap();
        assert_eq!(issue.title, "Bug");
        assert_eq!(issue.body, "Broken");
        assert!(issue.comments.is_empty());
    }

    #[test]
    fn parse_gh_issue_with_comments() {
        let raw = r#"{
            "title":"Add feature X",
            "body":"We need X.",
            "comments":[
                {"author":{"login":"alice"},"body":"Agreed","createdAt":"2025-01-02T10:00:00Z"},
                {"author":{"login":"bob"},"body":"+1","createdAt":"2025-01-03T11:00:00Z"}
            ]
        }"#;
        let issue = parse_gh_issue(raw).unwrap();
        assert_eq!(issue.title, "Add feature X");
        assert_eq!(issue.comments.len(), 2);
        assert_eq!(issue.comments[0].author.login, "alice");
        assert_eq!(issue.comments[0].created_at, "2025-01-02T10:00:00Z");
        assert_eq!(issue.comments[1].body, "+1");
    }

    #[test]
    fn parse_gh_issue_ignores_extra_fields() {
        let raw = r#"{
            "title":"T","body":"B","comments":[],
            "number":42,"state":"OPEN","author":{"login":"x"}
        }"#;
        let issue = parse_gh_issue(raw).unwrap();
        assert_eq!(issue.title, "T");
    }

    #[test]
    fn parse_gh_issue_invalid_json_errors() {
        let result = parse_gh_issue("not json {{{");
        assert!(matches!(result, Err(SkulkError::SshFailed(_))));
    }

    // ── wrap_file_prompt ───────────────────────────────────────────────

    #[test]
    fn wrap_file_prompt_includes_branch_and_content() {
        let out = wrap_file_prompt("skulk-my-task", "Fix the thing.");
        assert!(out.contains("skulk-my-task"));
        assert!(out.contains("Fix the thing."));
        assert!(out.contains("dedicated git worktree"));
        assert!(out.contains("ask me clarifying questions"));
        assert!(out.contains("---"));
    }

    #[test]
    fn wrap_file_prompt_multi_line_content_preserved() {
        let content = "Line 1\nLine 2\nLine 3";
        let out = wrap_file_prompt("skulk-t", content);
        assert!(out.contains("Line 1\nLine 2\nLine 3"));
    }

    // ── wrap_github_prompt ─────────────────────────────────────────────

    fn sample_issue() -> GhIssue {
        GhIssue {
            title: "Add feature X".into(),
            body: "We need X to do Y.".into(),
            comments: vec![GhComment {
                author: GhAuthor {
                    login: "alice".into(),
                },
                body: "Agreed, let's scope it.".into(),
                created_at: "2025-01-02T10:00:00Z".into(),
            }],
        }
    }

    #[test]
    fn wrap_github_prompt_contains_id_title_branch() {
        let out = wrap_github_prompt("42", "skulk-fix", &sample_issue());
        assert!(out.contains("#42"));
        assert!(out.contains("Add feature X"));
        assert!(out.contains("skulk-fix"));
        assert!(out.contains("gh` available"));
    }

    #[test]
    fn wrap_github_prompt_formats_comments_with_author_and_date() {
        let out = wrap_github_prompt("42", "skulk-fix", &sample_issue());
        assert!(out.contains("--- Comments (1) ---"));
        assert!(out.contains("alice (2025-01-02T10:00:00Z):"));
        assert!(out.contains("Agreed, let's scope it."));
    }

    #[test]
    fn wrap_github_prompt_no_comments_shows_zero_count() {
        let issue = GhIssue {
            title: "Quiet issue".into(),
            body: "Just me here.".into(),
            comments: vec![],
        };
        let out = wrap_github_prompt("7", "skulk-quiet", &issue);
        assert!(out.contains("--- Comments (0) ---"));
    }

    // ── gh_availability_command ────────────────────────────────────────

    #[test]
    fn gh_availability_command_checks_gh_and_auth() {
        let cmd = gh_availability_command();
        assert!(cmd.contains("command -v gh"));
        assert!(cmd.contains("gh auth status"));
        assert!(cmd.contains("SKULK_GH_MISSING"));
        assert!(cmd.contains("SKULK_GH_UNAUTHENTICATED"));
        assert!(cmd.contains("SKULK_GH_OK"));
    }

    // ── check_gh_available ─────────────────────────────────────────────

    #[test]
    fn check_gh_available_ok() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("SKULK_GH_OK".into())]);
        assert!(check_gh_available(&ssh, &cfg).is_ok());
    }

    #[test]
    fn check_gh_available_missing_returns_diagnostic() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("SKULK_GH_MISSING".into())]);
        let err = check_gh_available(&ssh, &cfg).unwrap_err();
        match err {
            SkulkError::Diagnostic {
                message,
                suggestion,
            } => {
                assert!(message.contains("not installed"));
                assert!(suggestion.contains("skulk init"));
            }
            other => panic!("expected Diagnostic, got {other}"),
        }
    }

    #[test]
    fn check_gh_available_unauthenticated_returns_diagnostic() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("SKULK_GH_UNAUTHENTICATED".into())]);
        let err = check_gh_available(&ssh, &cfg).unwrap_err();
        match err {
            SkulkError::Diagnostic {
                message,
                suggestion,
            } => {
                assert!(message.contains("not authenticated"));
                assert!(suggestion.contains("gh auth login"));
            }
            other => panic!("expected Diagnostic, got {other}"),
        }
    }

    // ── gh_issue_fetch_command ─────────────────────────────────────────

    #[test]
    fn gh_issue_fetch_command_uses_base_path_and_json() {
        let cfg = test_config();
        let cmd = gh_issue_fetch_command("42", &cfg);
        assert!(cmd.contains("cd ~/test-project"));
        assert!(cmd.contains("gh issue view 42"));
        assert!(cmd.contains("--json title,body,comments"));
    }

    // ── fetch_github_issue_raw ─────────────────────────────────────────

    #[test]
    fn fetch_github_issue_raw_passes_through_output() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(r#"{"title":"t","body":"b","comments":[]}"#.into())]);
        let out = fetch_github_issue_raw(&ssh, "42", &cfg).unwrap();
        assert!(out.contains("\"title\""));
    }

    #[test]
    fn fetch_github_issue_raw_maps_not_found_to_clean_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "GraphQL: Could not resolve to an Issue with the number of 999.".into(),
        ))]);
        let err = fetch_github_issue_raw(&ssh, "999", &cfg).unwrap_err();
        match err {
            SkulkError::NotFound(msg) => assert!(msg.contains("#999")),
            other => panic!("expected NotFound, got {other}"),
        }
    }

    #[test]
    fn fetch_github_issue_raw_passes_through_other_errors() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed(
            "some other failure".into(),
        ))]);
        let err = fetch_github_issue_raw(&ssh, "42", &cfg).unwrap_err();
        assert!(matches!(err, SkulkError::SshFailed(_)));
    }

    // ── load_github_prompt (end-to-end) ────────────────────────────────

    #[test]
    fn load_github_prompt_happy_path() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("SKULK_GH_OK".into()),
            Ok(r#"{"title":"T","body":"B","comments":[]}"#.into()),
        ]);
        let out = load_github_prompt(&ssh, "42", "skulk-x", &cfg).unwrap();
        assert!(out.contains("#42"));
        assert!(out.contains('T'));
        assert!(out.contains("skulk-x"));
    }

    #[test]
    fn load_github_prompt_rejects_invalid_id_before_ssh() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let err = load_github_prompt(&ssh, "not-a-number", "skulk-x", &cfg).unwrap_err();
        assert!(matches!(err, SkulkError::Validation(_)));
        // No SSH call made
        assert!(ssh.calls().is_empty());
    }

    #[test]
    fn load_github_prompt_surfaces_missing_gh() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("SKULK_GH_MISSING".into())]);
        let err = load_github_prompt(&ssh, "42", "skulk-x", &cfg).unwrap_err();
        assert!(matches!(err, SkulkError::Diagnostic { .. }));
    }

    // ── load_file_prompt ───────────────────────────────────────────────

    fn make_tmp_file(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("skulk_prompt_source_{name}"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_file_prompt_reads_and_wraps() {
        let path = make_tmp_file("reads_and_wraps.txt", "Do the thing.\n");
        let out = load_file_prompt(&path, "skulk-t").unwrap();
        assert!(out.contains("Do the thing."));
        assert!(out.contains("skulk-t"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_file_prompt_missing_file_errors() {
        let path = std::path::PathBuf::from("/nonexistent/skulk_prompt_absent.txt");
        let err = load_file_prompt(&path, "skulk-t").unwrap_err();
        assert!(matches!(err, SkulkError::Validation(_)));
    }

    #[test]
    fn load_file_prompt_empty_file_errors() {
        let path = make_tmp_file("empty.txt", "   \n\n  ");
        let err = load_file_prompt(&path, "skulk-t").unwrap_err();
        match err {
            SkulkError::Validation(msg) => assert!(msg.contains("empty")),
            other => panic!("expected Validation, got {other}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
