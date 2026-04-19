#[derive(Debug, thiserror::Error)]
pub(crate) enum SkulkError {
    /// SSH command could not be spawned
    #[error("Failed to run ssh: {0}")]
    SshExec(String),
    /// Diagnosed SSH failure with user-friendly message
    #[error("{message}\n  Suggestion: {suggestion}")]
    Diagnostic { message: String, suggestion: String },
    /// SSH succeeded but remote command failed (unrecognized stderr)
    #[error("SSH error: {0}")]
    SshFailed(String),
    /// Validation error (name, input)
    #[error("{0}")]
    Validation(String),
    /// Resource not found
    #[error("{0}")]
    NotFound(String),
}

pub(crate) fn classify_ssh_error(stderr: &str, host: &str) -> SkulkError {
    let lower = stderr.to_lowercase();
    classify_ssh_error_lower(stderr, &lower, host)
}

/// Same as `classify_ssh_error` but accepts a pre-lowercased copy of `stderr`.
/// Use this from callers that already hold a lowercased string to avoid
/// allocating a second one.
fn classify_ssh_error_lower(stderr: &str, lower: &str, host: &str) -> SkulkError {
    if lower.contains("connection timed out") || lower.contains("operation timed out") {
        SkulkError::Diagnostic {
            message: format!("Connection to {host} timed out."),
            suggestion: "Check your network connection and that the host is reachable.".into(),
        }
    } else if lower.contains("connection refused") {
        SkulkError::Diagnostic {
            message: format!("SSH connection refused by {host}."),
            suggestion: format!("Ensure SSH is running on {host}."),
        }
    } else if lower.contains("host key verification failed") {
        SkulkError::Diagnostic {
            message: format!("Host key verification failed for {host}."),
            suggestion: format!("Accept the host key first: ssh {host}"),
        }
    } else if lower.contains("permission denied") || lower.contains("authentication") {
        SkulkError::Diagnostic {
            message: "SSH authentication rejected.".into(),
            suggestion: format!("Check your SSH key: ssh {host} whoami"),
        }
    } else if lower.contains("could not resolve hostname") {
        SkulkError::Diagnostic {
            message: format!("Cannot resolve hostname '{host}'."),
            suggestion: "Check your network connection and DNS resolution.".into(),
        }
    } else if lower.contains("command not found") {
        SkulkError::Diagnostic {
            message: format!("Required command not found on {host}."),
            suggestion: format!("Ensure required tools (tmux, git, gh) are installed on {host}."),
        }
    } else {
        SkulkError::SshFailed(stderr.trim().to_string())
    }
}

/// Classify tmux/SSH errors with agent-specific context for better diagnostics.
///
/// When tmux reports "session not found" or "can't find session", produce a
/// friendly `NotFound` error mentioning the agent name. Otherwise, fall through
/// to standard SSH error classification.
pub(crate) fn classify_agent_error(name: &str, err: SkulkError, host: &str) -> SkulkError {
    match &err {
        SkulkError::SshFailed(stderr) => {
            let lower = stderr.to_lowercase();
            if lower.contains("session not found")
                || lower.contains("can't find session")
                || lower.contains("can't find pane")
                || lower.contains("unknown revision")
                || lower.contains("not a valid object name")
                || lower.contains("src refspec")
            {
                SkulkError::NotFound(format!(
                    "Agent '{name}' not found. Check running agents with `skulk list`."
                ))
            } else if lower.contains("does not appear to be a git repository")
                || lower.contains("no such remote")
            {
                // Note: "could not read from remote repository" is deliberately excluded —
                // git prints it as the trailing message for *any* remote-access failure
                // (timeouts, auth denials, etc.), not just missing-origin. Matching it
                // here would mask the real SSH error.
                SkulkError::Diagnostic {
                    message: "Remote 'origin' is not configured on the base repository.".into(),
                    suggestion: format!(
                        "Configure origin on {host}: `git -C <base_path> remote add origin <url>`"
                    ),
                }
            } else {
                classify_ssh_error_lower(stderr, &lower, host)
            }
        }
        _ => err,
    }
}

pub(crate) fn is_tmux_no_server(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("no server running") || lower.contains("no sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::assert_err;

    #[test]
    fn classify_ssh_error_connection_timed_out() {
        let err: Result<(), _> = Err(classify_ssh_error(
            "ssh: connect to host bluebubble: Connection timed out",
            "testhost",
        ));
        assert_err!(err, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("timed out"));
            assert!(suggestion.contains("network connection"));
        });
    }

    #[test]
    fn classify_ssh_error_operation_timed_out() {
        let err: Result<(), _> = Err(classify_ssh_error(
            "ssh: connect to host bluebubble: Operation timed out",
            "testhost",
        ));
        assert_err!(err, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("timed out"));
            assert!(suggestion.contains("network connection"));
        });
    }

    #[test]
    fn classify_ssh_error_connection_refused() {
        let err: Result<(), _> = Err(classify_ssh_error(
            "ssh: connect to host bluebubble port 22: Connection refused",
            "testhost",
        ));
        assert_err!(err, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("refused"));
            assert!(suggestion.contains("Ensure SSH is running"));
        });
    }

    #[test]
    fn classify_ssh_error_host_key_verification_failed() {
        let err: Result<(), _> = Err(classify_ssh_error(
            "Host key verification failed.",
            "testhost",
        ));
        assert_err!(err, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("Host key verification failed"));
            assert!(suggestion.contains("Accept the host key"));
        });
    }

    #[test]
    fn classify_ssh_error_permission_denied() {
        let err: Result<(), _> = Err(classify_ssh_error(
            "Permission denied (publickey)",
            "testhost",
        ));
        assert_err!(err, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("authentication"));
            assert!(suggestion.contains("ssh testhost whoami"));
        });
    }

    #[test]
    fn classify_ssh_error_cannot_resolve() {
        let err: Result<(), _> = Err(classify_ssh_error(
            "ssh: Could not resolve hostname bluebubble",
            "testhost",
        ));
        assert_err!(err, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("resolve"));
            assert!(suggestion.contains("DNS resolution"));
        });
    }

    #[test]
    fn classify_ssh_error_command_not_found() {
        let err: Result<(), _> = Err(classify_ssh_error(
            "bash: tmux: command not found",
            "testhost",
        ));
        assert_err!(err, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.contains("not found"));
            assert!(suggestion.contains("required tools"));
            assert!(suggestion.contains("testhost"));
        });
    }

    #[test]
    fn classify_ssh_error_unknown_returns_ssh_failed() {
        let err: Result<(), _> = Err(classify_ssh_error("some unknown error text", "testhost"));
        assert_err!(err, SkulkError::SshFailed(msg) => {
            assert_eq!(msg, "some unknown error text");
        });
    }

    #[test]
    fn is_tmux_no_server_no_server_running() {
        assert!(is_tmux_no_server(
            "no server running on /tmp/tmux-1000/default"
        ));
    }

    #[test]
    fn is_tmux_no_server_no_sessions() {
        assert!(is_tmux_no_server("error: no sessions"));
    }

    #[test]
    fn is_tmux_no_server_other_error() {
        assert!(!is_tmux_no_server("some other error"));
    }

    #[test]
    fn classify_agent_error_session_not_found() {
        let err = SkulkError::SshFailed("can't find session: skulk-foo".to_string());
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("foo"));
            assert!(msg.contains("not found"));
        });
    }

    #[test]
    fn classify_agent_error_session_not_found_variant() {
        let err = SkulkError::SshFailed("session not found: skulk-bar".to_string());
        let result: Result<(), _> = Err(classify_agent_error("bar", err, "testhost"));
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("bar"));
        });
    }

    #[test]
    fn classify_agent_error_unknown_revision_returns_not_found() {
        let err = SkulkError::SshFailed(
            "fatal: ambiguous argument 'main...skulk-foo': unknown revision or path not in the working tree"
                .to_string(),
        );
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("foo"));
            assert!(msg.contains("not found"));
        });
    }

    #[test]
    fn classify_agent_error_not_a_valid_object_name_returns_not_found() {
        let err = SkulkError::SshFailed("fatal: Not a valid object name skulk-foo".to_string());
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("foo"));
        });
    }

    #[test]
    fn classify_agent_error_src_refspec_returns_not_found() {
        let err =
            SkulkError::SshFailed("error: src refspec skulk-foo does not match any".to_string());
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("foo"));
        });
    }

    #[test]
    fn classify_agent_error_origin_missing_returns_diagnostic() {
        let err = SkulkError::SshFailed(
            "fatal: 'origin' does not appear to be a git repository".to_string(),
        );
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::Diagnostic { message, suggestion } => {
            assert!(message.to_lowercase().contains("origin"));
            assert!(suggestion.contains("testhost"));
        });
    }

    #[test]
    fn classify_agent_error_no_such_remote_returns_diagnostic() {
        let err = SkulkError::SshFailed("fatal: No such remote 'origin'".to_string());
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::Diagnostic { .. } => {});
    }

    #[test]
    fn classify_agent_error_push_connection_timeout_not_misclassified() {
        // Regression: when a push fails due to network timeout, git emits both
        // "Connection timed out" (real cause) and "Could not read from remote
        // repository" (trailing generic message). The latter must not trigger
        // the origin-missing diagnostic — the timeout must surface instead.
        let err = SkulkError::SshFailed(
            "ssh: connect to host github.com port 22: Connection timed out\n\
             fatal: Could not read from remote repository."
                .to_string(),
        );
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(
                message.to_lowercase().contains("timed out"),
                "expected timeout diagnostic, got: {message}"
            );
            assert!(
                !message.to_lowercase().contains("origin"),
                "timeout must not be reported as origin-missing: {message}"
            );
        });
    }

    #[test]
    fn classify_agent_error_push_permission_denied_not_misclassified() {
        // Regression: GitHub permission denial also prints "Could not read from
        // remote repository." Must classify as auth failure, not origin-missing.
        let err = SkulkError::SshFailed(
            "git@github.com: Permission denied (publickey).\n\
             fatal: Could not read from remote repository."
                .to_string(),
        );
        let result: Result<(), _> = Err(classify_agent_error("foo", err, "testhost"));
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(
                message.to_lowercase().contains("authentication"),
                "expected auth diagnostic, got: {message}"
            );
        });
    }

    #[test]
    fn classify_agent_error_pane_not_found() {
        let err = SkulkError::SshFailed("can't find pane: skulk-nope".to_string());
        let result: Result<(), _> = Err(classify_agent_error("nope", err, "testhost"));
        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("nope"));
            assert!(msg.contains("not found"));
        });
    }

    #[test]
    fn classify_agent_error_ssh_error_passthrough() {
        let err = SkulkError::SshFailed("Connection timed out".to_string());
        let result: Result<(), _> = Err(classify_agent_error("baz", err, "testhost"));
        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.contains("timed out"));
        });
    }

    #[test]
    fn classify_agent_error_non_ssh_passthrough() {
        let err = SkulkError::Validation("bad name".to_string());
        let result: Result<(), _> = Err(classify_agent_error("whatever", err, "testhost"));
        assert_err!(result, SkulkError::Validation(msg) => {
            assert_eq!(msg, "bad name");
        });
    }

    #[test]
    fn skulk_error_validation_display() {
        let err = SkulkError::Validation("Name too long.".into());
        assert_eq!(format!("{err}"), "Name too long.");
    }

    #[test]
    fn skulk_error_not_found_display() {
        let err = SkulkError::NotFound("Agent 'foo' not found.".into());
        assert_eq!(format!("{err}"), "Agent 'foo' not found.");
    }
}
