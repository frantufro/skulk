use std::io::BufRead;

use crate::error::SkulkError;

pub(crate) const STARTUP_DELAY: u32 = 5;

/// Check whether a host refers to the local machine.
///
/// When true, commands run locally via `sh -c` instead of over SSH.
/// Exact-match only: aliases like `localhost.localdomain`, `[::1]`, or
/// other 127.0.0.0/8 addresses are not recognized. Extend if users ask.
pub(crate) fn is_localhost(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Validate an agent name: [a-z0-9-], 1-30 chars,
/// no leading/trailing/consecutive hyphens.
pub(crate) fn validate_name(name: &str) -> Result<(), SkulkError> {
    if name.is_empty() {
        return Err(SkulkError::Validation("Agent name cannot be empty.".into()));
    }
    if name.len() > 30 {
        return Err(SkulkError::Validation(
            "Agent name must be 30 characters or fewer.".into(),
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(SkulkError::Validation(
            "Agent name cannot start or end with a hyphen.".into(),
        ));
    }
    if name.contains("--") {
        return Err(SkulkError::Validation(
            "Agent name cannot contain consecutive hyphens.".into(),
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(SkulkError::Validation(format!(
                "Invalid character '{c}' in agent name. Only lowercase letters, digits, and hyphens allowed."
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
/// string if either marker is missing.
pub(crate) fn extract_section(raw: &str, start_marker: &str, end_marker: &str) -> String {
    let start = raw.find(start_marker).map(|i| i + start_marker.len());
    let end = raw.find(end_marker);
    match (start, end) {
        (Some(s), Some(e)) if s < e => raw[s..e].to_string(),
        _ => String::new(),
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

/// Find the index in `new_lines` where new content begins, relative to `old_lines`.
///
/// Uses suffix-matching: finds the longest suffix of `old_lines` that appears as a
/// contiguous subsequence in `new_lines`. Everything after that match is new content.
/// Returns 0 if no overlap (show all), or `new_lines.len()` if nothing changed.
///
/// Complexity is O(n^2 * m) but bounded by the follow buffer size (200 lines).
pub(crate) fn find_new_content_start(old_lines: &[String], new_lines: &[String]) -> usize {
    if old_lines.is_empty() {
        return 0;
    }

    // Try progressively shorter suffixes of old_lines
    for start in 0..old_lines.len() {
        let suffix = &old_lines[start..];
        let suffix_len = suffix.len();

        // Check if this suffix appears as a contiguous block in new_lines
        if suffix_len <= new_lines.len() {
            for new_start in 0..=new_lines.len() - suffix_len {
                if new_lines[new_start..new_start + suffix_len] == *suffix {
                    // Found match -- new content starts after the match
                    return new_start + suffix_len;
                }
            }
        }
    }

    // No overlap found -- all content is new
    0
}

/// Result of delivering a prompt to a newly-created agent.
pub(crate) enum PromptStatus {
    Delivered,
    Failed,
    NotSent,
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
        let name = "abcdefghijklmnopqrstuvwxyz1234"; // exactly 30 chars
        assert_eq!(name.len(), 30);
        assert!(validate_name(name).is_ok());
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
        let name = "a".repeat(31);
        let result = validate_name(&name);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("30 characters"));
    }

    #[test]
    fn validate_name_uppercase() {
        let result = validate_name("My-Task");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("Invalid character"));
    }

    #[test]
    fn validate_name_underscore() {
        let result = validate_name("my_task");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("Invalid character"));
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
        assert!(msg.contains("start or end"));
    }

    #[test]
    fn validate_name_trailing_hyphen() {
        let result = validate_name("trailing-");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("start or end"));
    }

    #[test]
    fn validate_name_consecutive_hyphens() {
        let result = validate_name("double--hyphen");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("consecutive"));
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
