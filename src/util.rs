use std::io::BufRead;

use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;

/// Verify the remote base clone exists (`{base_path}/.git` directory present).
///
/// Each caller supplies its own `Validation` message via `missing_msg` because
/// `skulk pull` and `skulk new` guide the user toward different next steps when
/// the clone is absent.
///
/// `SshFailed` is interpreted as "directory missing" (the `test -d` command
/// returned non-zero). Other errors — connectivity, auth, DNS — are propagated
/// unchanged so the user sees the real cause.
pub(crate) fn check_base_clone(
    ssh: &impl Ssh,
    cfg: &Config,
    missing_msg: impl FnOnce() -> String,
) -> Result<(), SkulkError> {
    let base_path = &cfg.base_path;
    match ssh.run(&format!("test -d {base_path}/.git && echo exists")) {
        Ok(_) => Ok(()),
        Err(SkulkError::SshFailed(_)) => Err(SkulkError::Validation(missing_msg())),
        Err(e) => Err(e),
    }
}

/// Validate a model identifier: `[A-Za-z0-9._/-]`, 1-64 chars.
///
/// Matches the shape of real Claude model IDs (`opus`, `sonnet`,
/// `claude-opus-4-7`, etc.) and `OpenCode`'s `provider/model` format
/// (`anthropic/claude-opus-4-7`) while rejecting shell metacharacters. This
/// matters because the model string is typed into the remote tmux shell by
/// `send-keys`, which would otherwise re-evaluate characters like `;`, `$`,
/// or backticks.
pub(crate) fn validate_model(model: &str) -> Result<(), SkulkError> {
    if model.is_empty() {
        return Err(SkulkError::Validation("Model name cannot be empty.".into()));
    }
    if model.len() > 64 {
        return Err(SkulkError::Validation(
            "Model name must be 64 characters or fewer.".into(),
        ));
    }
    for c in model.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/') {
            return Err(SkulkError::Validation(format!(
                "Invalid character '{c}' in model name. Only letters, digits, hyphens, underscores, dots, and slashes allowed."
            )));
        }
    }
    Ok(())
}

/// Validate an agent name: [a-zA-Z0-9/_-], 1-100 chars.
pub(crate) fn validate_name(name: &str) -> Result<(), SkulkError> {
    if name.is_empty() {
        return Err(SkulkError::Validation("Agent name cannot be empty.".into()));
    }
    if name.len() > 100 {
        return Err(SkulkError::Validation(
            "Agent name must be 100 characters or fewer.".into(),
        ));
    }
    if name.starts_with('-') {
        return Err(SkulkError::Validation(
            "Agent name cannot start with a hyphen.".into(),
        ));
    }
    if name.starts_with('/') {
        return Err(SkulkError::Validation(
            "Agent name cannot start with a slash.".into(),
        ));
    }
    if name.ends_with('/') {
        return Err(SkulkError::Validation(
            "Agent name cannot end with a slash.".into(),
        ));
    }
    if name.contains("//") {
        return Err(SkulkError::Validation(
            "Agent name cannot contain consecutive slashes.".into(),
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/') {
            return Err(SkulkError::Validation(format!(
                "Invalid character '{c}' in agent name. Only letters, digits, hyphens, underscores, and slashes allowed."
            )));
        }
    }
    Ok(())
}

/// POSIX single-quote escape: replace `'` with `'\''` for safe SSH -> tmux send-keys transit.
///
/// The caller wraps the result in single quotes: `format!("'{}'", shell_escape(input))`.
/// Inside POSIX single-quoted strings, ALL characters are literal except the single quote
/// itself. That means backticks, `$`, `\`, spaces, newlines, etc. need no escaping.
/// Only single quotes need the close-escape-reopen trick: `'\''`.
pub(crate) fn shell_escape(input: &str) -> String {
    input.replace('\'', "'\\''")
}

/// Extract a delimited section from raw SSH output.
///
/// Returns the content between `start_marker` and `end_marker`, or an empty
/// slice if either marker is missing.
pub(crate) fn extract_section<'a>(raw: &'a str, start: &str, end: &str) -> &'a str {
    let s = raw.find(start).map(|i| i + start.len());
    let e = raw.find(end);
    match (s, e) {
        (Some(s), Some(e)) if s < e => &raw[s..e],
        _ => "",
    }
}

/// Read a yes/no confirmation from the given reader. Returns true for "y" or "yes" (case-insensitive).
/// Returns false on EOF or any other input.
pub(crate) fn confirm_from_reader<R: BufRead>(prompt: &str, reader: &mut R) -> bool {
    eprint!("{prompt} ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return false; // EOF
    }
    let answer = line.trim().to_lowercase();
    answer == "y" || answer == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_name tests ─────────────────────────────────────────────

    #[test]
    fn validate_name_valid_simple() {
        assert!(validate_name("my-task").is_ok());
    }

    #[test]
    fn validate_name_valid_digits() {
        assert!(validate_name("fix-123").is_ok());
    }

    #[test]
    fn validate_name_valid_single_char() {
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn validate_name_valid_max_length() {
        let name = "a".repeat(100);
        assert!(validate_name(&name).is_ok());
    }

    #[test]
    fn validate_name_empty() {
        let result = validate_name("");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("empty"), "error should mention 'empty': {msg}");
    }

    #[test]
    fn validate_name_too_long() {
        let name = "a".repeat(101);
        let result = validate_name(&name);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("100 characters"));
    }

    #[test]
    fn validate_name_uppercase() {
        assert!(validate_name("My-Task").is_ok());
    }

    #[test]
    fn validate_name_underscore() {
        assert!(validate_name("my_task").is_ok());
    }

    #[test]
    fn validate_name_space() {
        let result = validate_name("my task");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("Invalid character"));
    }

    #[test]
    fn validate_name_leading_hyphen() {
        let result = validate_name("-leading");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("start with a hyphen"));
    }

    #[test]
    fn validate_name_valid_slash() {
        assert!(validate_name("feat/add-feature").is_ok());
    }

    #[test]
    fn validate_name_valid_namespaced_slash() {
        assert!(validate_name("feat/fix/deep").is_ok());
    }

    #[test]
    fn validate_name_leading_slash_rejected() {
        let result = validate_name("/absolute");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("start with a slash"));
    }

    #[test]
    fn validate_name_trailing_slash_rejected() {
        let result = validate_name("feat/");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("end with a slash"));
    }

    #[test]
    fn validate_name_consecutive_slashes_rejected() {
        let result = validate_name("feat//thing");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("consecutive slashes"));
    }

    #[test]
    fn validate_name_valid_100_chars() {
        let name = "a".repeat(100);
        assert!(validate_name(&name).is_ok());
    }

    #[test]
    fn validate_name_101_chars_rejected() {
        let name = "a".repeat(101);
        let result = validate_name(&name);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("100 characters"));
    }

    // ── shell_escape tests ──────────────────────────────────────────────

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape("hello"), "hello");
    }

    #[test]
    fn shell_escape_single_quote() {
        assert_eq!(shell_escape("it's"), "it'\\''s");
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "");
    }

    #[test]
    fn shell_escape_backticks_unchanged() {
        assert_eq!(shell_escape("hello `world`"), "hello `world`");
    }

    #[test]
    fn shell_escape_dollar_unchanged() {
        assert_eq!(shell_escape("$HOME/path"), "$HOME/path");
    }

    // ── validate_model tests ────────────────────────────────────────────

    #[test]
    fn validate_model_valid_short_alias() {
        assert!(validate_model("opus").is_ok());
        assert!(validate_model("sonnet").is_ok());
    }

    #[test]
    fn validate_model_valid_full_id() {
        assert!(validate_model("claude-opus-4-7").is_ok());
        assert!(validate_model("claude-sonnet-4-6").is_ok());
    }

    #[test]
    fn validate_model_valid_with_underscore_and_dot() {
        assert!(validate_model("claude_4.7").is_ok());
    }

    #[test]
    fn validate_model_valid_provider_slash_model() {
        // OpenCode requires `provider/model`.
        assert!(validate_model("anthropic/claude-opus-4-7").is_ok());
        assert!(validate_model("openai/gpt-4o").is_ok());
    }

    #[test]
    fn validate_model_empty() {
        let result = validate_model("");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("empty"));
    }

    #[test]
    fn validate_model_too_long() {
        let m = "a".repeat(65);
        let result = validate_model(&m);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("64 characters"));
    }

    #[test]
    fn validate_model_rejects_semicolon() {
        let result = validate_model("opus; rm -rf /");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("Invalid character"));
    }

    #[test]
    fn validate_model_rejects_whitespace() {
        assert!(validate_model("opus sonnet").is_err());
    }

    #[test]
    fn validate_model_rejects_single_quote() {
        assert!(validate_model("it's").is_err());
    }

    #[test]
    fn validate_model_rejects_command_substitution() {
        assert!(validate_model("$(whoami)").is_err());
        assert!(validate_model("`id`").is_err());
    }

    // ── confirm tests ───────────────────────────────────────────────────

    #[test]
    fn confirm_y() {
        let mut input = std::io::Cursor::new(b"y\n");
        assert!(confirm_from_reader("Delete?", &mut input));
    }

    #[test]
    fn confirm_yes() {
        let mut input = std::io::Cursor::new(b"yes\n");
        assert!(confirm_from_reader("Delete?", &mut input));
    }

    #[test]
    fn confirm_n() {
        let mut input = std::io::Cursor::new(b"n\n");
        assert!(!confirm_from_reader("Delete?", &mut input));
    }

    #[test]
    fn confirm_empty() {
        let mut input = std::io::Cursor::new(b"\n");
        assert!(!confirm_from_reader("Delete?", &mut input));
    }

    #[test]
    fn confirm_eof_returns_false() {
        let mut input = std::io::Cursor::new(b"");
        assert!(!confirm_from_reader("Delete?", &mut input));
    }
}
