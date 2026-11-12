//! `skulk logs --follow`: poll the agent pane and print only new lines.

use std::io::Write;
use std::time::Duration;

use crate::agent::Agent;
use crate::commands::interact::logs_snapshot_deep_command;
use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

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

/// Follow agent output in real-time via poll loop. Runs until interrupted.
pub(crate) fn cmd_logs_follow(
    ssh: &impl Ssh,
    agent: &Agent,
    cfg: &Config,
) -> Result<(), SkulkError> {
    follow_logs(
        ssh,
        agent,
        cfg,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &mut || {
            std::thread::sleep(POLL_INTERVAL);
            true
        },
    )
}

/// Poll loop behind `cmd_logs_follow`. `wait` runs before each poll and
/// returns `false` to end the loop; production passes a sleep that always
/// returns `true`.
fn follow_logs(
    ssh: &impl Ssh,
    agent: &Agent,
    cfg: &Config,
    out: &mut dyn Write,
    err: &mut dyn Write,
    wait: &mut dyn FnMut() -> bool,
) -> Result<(), SkulkError> {
    let name = agent.name();
    let cmd = logs_snapshot_deep_command(name, 200, cfg);
    let initial = ssh.run(&cmd).map_err(|e| agent.classify(e))?;
    let mut last_lines: Vec<String> = initial.lines().map(ToString::to_string).collect();
    let _ = writeln!(err, "Following {name} (Ctrl+C to stop)...");
    for line in &last_lines {
        let _ = writeln!(out, "{line}");
    }
    while wait() {
        match ssh.run(&cmd) {
            Ok(output) => {
                let current: Vec<String> = output.lines().map(ToString::to_string).collect();
                let new_start = find_new_content_start(&last_lines, &current);
                for line in &current[new_start..] {
                    let _ = writeln!(out, "{line}");
                }
                last_lines = current;
            }
            Err(e) => {
                let _ = writeln!(err, "Warning: capture failed: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, ssh_err, test_config};

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    fn run_follow(
        responses: Vec<Result<String, SkulkError>>,
        polls: usize,
    ) -> (Result<(), SkulkError>, String, String) {
        let ssh = MockSsh::new(responses);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut remaining = polls;
        let cfg = test_config();
        let agent = Agent::resolve("alpha", &cfg).unwrap();
        let result = follow_logs(&ssh, &agent, &cfg, &mut out, &mut err, &mut || {
            let go = remaining > 0;
            remaining = remaining.saturating_sub(1);
            go
        });
        (
            result,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    #[test]
    fn follow_prints_initial_snapshot_and_banner() {
        let (result, out, err) = run_follow(vec![Ok("a\nb\n".into())], 0);

        assert!(result.is_ok());
        assert_eq!(out, "a\nb\n");
        assert!(err.contains("Following alpha (Ctrl+C to stop)..."));
    }

    #[test]
    fn follow_prints_only_new_lines_on_each_poll() {
        let (result, out, _) = run_follow(
            vec![
                Ok("a\nb\n".into()),
                Ok("b\nc\n".into()),
                Ok("b\nc\nd\n".into()),
            ],
            2,
        );

        assert!(result.is_ok());
        assert_eq!(out, "a\nb\nc\nd\n");
    }

    #[test]
    fn follow_warns_and_continues_when_a_poll_fails() {
        let (result, out, err) = run_follow(
            vec![Ok("a\n".into()), ssh_err("boom"), Ok("a\nb\n".into())],
            2,
        );

        assert!(result.is_ok());
        assert_eq!(out, "a\nb\n");
        assert!(err.contains("Warning: capture failed"));
    }

    #[test]
    fn follow_returns_error_when_initial_capture_fails() {
        let (result, out, _) = run_follow(vec![ssh_err("boom")], 0);

        assert!(result.is_err());
        assert_eq!(out, "");
    }

    #[test]
    fn find_new_content_start_partial_overlap() {
        let old = lines(&["a", "b", "c"]);
        let new = lines(&["b", "c", "d"]);
        assert_eq!(find_new_content_start(&old, &new), 2);
    }

    #[test]
    fn find_new_content_start_no_change() {
        let old = lines(&["a", "b"]);
        assert_eq!(find_new_content_start(&old, &old), 2);
    }

    #[test]
    fn find_new_content_start_complete_change() {
        let old = lines(&["a", "b"]);
        let new = lines(&["x", "y", "z"]);
        assert_eq!(find_new_content_start(&old, &new), 0);
    }

    #[test]
    fn find_new_content_start_empty_old() {
        let new = lines(&["a", "b"]);
        assert_eq!(find_new_content_start(&[], &new), 0);
    }

    #[test]
    fn find_new_content_start_suffix_match_at_last_iteration() {
        let old = lines(&["x", "y", "a"]);
        let new = lines(&["b", "c", "a", "d"]);
        assert_eq!(find_new_content_start(&old, &new), 3);
    }

    #[test]
    fn find_new_content_start_empty_new() {
        let old = lines(&["a", "b"]);
        assert_eq!(find_new_content_start(&old, &[]), 0);
    }

    #[test]
    fn find_new_content_start_partial_match_not_contiguous() {
        let old = lines(&["a", "b", "c"]);
        let new = lines(&["a", "x", "c", "d"]);
        assert_eq!(find_new_content_start(&old, &new), 3);
    }
}
