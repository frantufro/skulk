//! Confirming that a prompt skulk typed or pasted was actually submitted.
//!
//! A large bracketed paste can still be landing in the harness's input box
//! when the submitting Enter arrives, and the harness then drops the Enter.
//! The prompt sits unsent while skulk reports it delivered. So after Enter,
//! skulk watches for proof of submission and presses Enter again a bounded
//! number of times before it gives up and reports the prompt as not submitted.

use std::time::Duration;

use crate::config::{Config, DEFAULT_HARNESS};
use crate::remote::resolve_agent_window;
use crate::ssh::Ssh;

/// Separates the marker from the pane capture in the probe output.
const PANE_DELIMITER: &str = "__SKULK_PANE__";

/// Pane text a harness shows only while it is working on a turn: Claude
/// Code's `esc to interrupt` and `OpenCode`'s `esc interrupt`.
const WORKING_INDICATORS: &[&str] = &["esc to interrupt", "esc interrupt"];

/// Lines above the input box a harness uses for its status (spinner, elapsed
/// time, `esc to interrupt`).
const STATUS_LINES: usize = 3;

/// Lines a harness shows below its input box (mode, shortcuts hint).
const MAX_FOOTER_LINES: usize = 4;

/// Settle delay before the first Enter for a prompt of zero bytes.
const SETTLE_BASE_MS: u64 = 200;
/// Extra settle delay per KiB of prompt.
const SETTLE_PER_KIB_MS: u64 = 100;
/// Upper bound on the settle delay.
const SETTLE_MAX_MS: u64 = 5_000;

/// Whether the prompt left the input box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Submission {
    Submitted,
    NotSubmitted,
}

/// How long and how often to look for proof of submission.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SubmitPacing {
    /// Pause before each probe.
    pub poll_interval: Duration,
    /// Probes after each Enter before the next Enter.
    pub polls_per_enter: u32,
    /// Enters in total, counting the one sent with the prompt.
    pub max_enters: u32,
}

impl SubmitPacing {
    /// About three seconds per Enter, three Enters.
    pub(crate) fn production() -> Self {
        Self {
            poll_interval: Duration::from_millis(500),
            polls_per_enter: 6,
            max_enters: 3,
        }
    }

    #[cfg(test)]
    pub(crate) fn zero() -> Self {
        Self {
            poll_interval: Duration::ZERO,
            polls_per_enter: 1,
            max_enters: 3,
        }
    }
}

/// How long to let a paste land before pressing Enter, scaled to its size.
pub(crate) fn settle_delay(prompt_len: usize) -> Duration {
    let kib = u64::try_from(prompt_len / 1024).unwrap_or(u64::MAX);
    let ms = SETTLE_BASE_MS
        .saturating_add(kib.saturating_mul(SETTLE_PER_KIB_MS))
        .min(SETTLE_MAX_MS);
    Duration::from_millis(ms)
}

/// `delay` as an argument for the remote `sleep`, in seconds.
pub(crate) fn sleep_arg(delay: Duration) -> String {
    format!("{}.{:03}", delay.as_secs(), delay.subsec_millis())
}

/// Build the SSH command that prints the idle marker, a delimiter line, then
/// the agent window's visible pane.
fn probe_command(session_name: &str) -> String {
    let resolve = resolve_agent_window(session_name);
    format!(
        "{resolve} && {{ cat ~/.skulk/state/{session_name} 2>/dev/null || echo missing; }} && \
         echo {PANE_DELIMITER} && tmux capture-pane -p -t \"$skulk_win\""
    )
}

/// Build the SSH command that presses Enter in the agent window again.
fn resend_enter_command(session_name: &str) -> String {
    let resolve = resolve_agent_window(session_name);
    format!("{resolve} && tmux send-keys -t \"$skulk_win\" Enter")
}

/// A line drawn only with box-drawing characters: an input box's top or
/// bottom edge.
fn is_border(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| matches!(c, '─' | '━' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘'))
}

/// `line` without a box's side edges and the padding inside them.
fn strip_box(line: &str) -> &str {
    let edge = |c: char| c.is_whitespace() || matches!(c, '│' | '┃');
    line.trim_start_matches(edge).trim_end_matches(edge)
}

/// The text after the input prompt (`>` or `❯`), when `line` starts with one.
fn after_prompt_char(line: &str) -> Option<&str> {
    let t = strip_box(line);
    t.strip_prefix('>')
        .or_else(|| t.strip_prefix('❯'))
        .map(str::trim)
}

/// A harness pane split into the parts the submission check reads.
struct PaneView<'a> {
    /// The few lines just above the input box, where a harness shows that it
    /// is working. Without an input box, the last non-empty lines.
    status: Vec<&'a str>,
}

impl<'a> PaneView<'a> {
    /// Split `pane`. The input box is the last prompt line that sits right
    /// under a border, with a closing border below it and at most a short
    /// footer after that. Requiring the borders keeps a `> ...` line in the
    /// transcript (a past user message) from being taken for the input box.
    fn parse(pane: &'a str) -> Self {
        let mut lines: Vec<&str> = pane.lines().collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let above = match find_input_box(&lines) {
            Some((top, _, _)) => &lines[..top],
            None => &lines[..],
        };
        let status = above
            .iter()
            .rev()
            .filter(|l| !l.trim().is_empty() && !is_border(l))
            .take(STATUS_LINES)
            .copied()
            .collect();
        Self { status }
    }

    /// Whether the status lines show the harness working on a turn.
    fn shows_working(&self) -> bool {
        self.status
            .iter()
            .any(|l| WORKING_INDICATORS.iter().any(|w| l.contains(w)))
    }
}

/// Locate the input box as (top border, prompt line, bottom border) indices.
fn find_input_box(lines: &[&str]) -> Option<(usize, usize, usize)> {
    (1..lines.len()).rev().find_map(|prompt| {
        if after_prompt_char(lines[prompt]).is_none() || !is_border(lines[prompt - 1]) {
            return None;
        }
        let bottom = (prompt + 1..lines.len()).find(|&i| is_border(lines[i]))?;
        (lines.len() - 1 - bottom <= MAX_FOOTER_LINES).then_some((prompt - 1, prompt, bottom))
    })
}

/// Whether one probe shows the prompt was submitted.
///
/// For Claude Code, the `UserPromptSubmit` hook replacing skulk's `pending`
/// marker (with `busy`, or already `idle` after a short turn) proves it.
/// `OpenCode` has no such hook, so only the pane counts there. The working
/// indicator counts only on the status lines: the prompt itself, still
/// stuck in the input box, may well contain the same words.
fn probe_shows_submission(raw: &str, cfg: &Config) -> bool {
    let (marker, pane) = raw.split_once(PANE_DELIMITER).unwrap_or((raw, ""));
    let marker = marker.trim();
    let hook_fired = matches!(marker, "busy" | "idle");
    PaneView::parse(pane).shows_working() || (cfg.harness == DEFAULT_HARNESS && hook_fired)
}

/// Watch for proof that the prompt just sent with Enter was submitted,
/// pressing Enter again while there is none, up to `pacing.max_enters`.
///
/// A failed probe or resend counts as no proof yet rather than as an error:
/// the prompt is already in the harness's input box, and the caller reports
/// what happened instead of failing.
pub(crate) fn confirm_submission(
    ssh: &impl Ssh,
    session_name: &str,
    cfg: &Config,
    pacing: &SubmitPacing,
) -> Submission {
    let probe = probe_command(session_name);
    for enter in 0..pacing.max_enters.max(1) {
        if enter > 0 {
            let _ = ssh.run(&resend_enter_command(session_name));
        }
        for _ in 0..pacing.polls_per_enter.max(1) {
            std::thread::sleep(pacing.poll_interval);
            if ssh
                .run(&probe)
                .is_ok_and(|raw| probe_shows_submission(&raw, cfg))
            {
                return Submission::Submitted;
            }
        }
    }
    Submission::NotSubmitted
}

/// A probe frame for `MockSsh`: what [`probe_command`] prints.
///
/// Wrapped in `Ok` so it sits next to `ssh_ok()` in `MockSsh::new(vec![...])`.
#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn probe_frame(marker: &str, pane: &str) -> Result<String, crate::error::SkulkError> {
    Ok(format!("{marker}\n{PANE_DELIMITER}\n{pane}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, ssh_err, ssh_ok, test_config};

    fn opencode_config() -> Config {
        Config {
            harness: "opencode".into(),
            ..test_config()
        }
    }

    /// Claude Code with a large paste still sitting in its input box.
    const STUCK_PANE: &str = "⏺ Ready when you are.\n\n\
        ────────────────────────────────\n\
        > [Pasted text #1 +79 lines]\n\
        \x20 ↑ 79 more\n\
        ────────────────────────────────\n\
        \x20 ⏵⏵ bypass permissions on (shift+tab to cycle)\n";

    /// Claude Code working on a turn, input box empty.
    const WORKING_PANE: &str = "> fix the bug\n\n\
        ✻ Thinking… (3s · esc to interrupt)\n\n\
        ────────────────────────────────\n\
        >\n\
        ────────────────────────────────\n\
        \x20 ? for shortcuts\n";

    #[test]
    fn settle_delay_grows_with_prompt_size() {
        assert_eq!(settle_delay(10), Duration::from_millis(200));
        assert_eq!(settle_delay(10 * 1024), Duration::from_millis(1_200));
    }

    #[test]
    fn settle_delay_is_capped() {
        assert_eq!(settle_delay(10 * 1024 * 1024), Duration::from_secs(5));
    }

    #[test]
    fn sleep_arg_prints_seconds_with_millis() {
        assert_eq!(sleep_arg(Duration::from_millis(1_200)), "1.200");
        assert_eq!(sleep_arg(Duration::from_millis(50)), "0.050");
    }

    #[test]
    fn probe_command_prints_marker_then_pane_of_agent_window() {
        let cmd = probe_command("skulk-my-task");
        let marker = cmd.find("~/.skulk/state/skulk-my-task").unwrap();
        let delimiter = cmd.find(PANE_DELIMITER).unwrap();
        let capture = cmd.find("tmux capture-pane -p -t \"$skulk_win\"").unwrap();
        assert!(marker < delimiter && delimiter < capture, "{cmd}");
        assert!(cmd.starts_with("skulk_win=$(tmux list-windows -t skulk-my-task"));
    }

    #[test]
    fn submitted_when_hook_flips_pending_to_busy() {
        let ssh = MockSsh::new(vec![probe_frame("busy", STUCK_PANE)]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &SubmitPacing::zero());

        assert_eq!(result, Submission::Submitted);
        assert_eq!(ssh.calls().len(), 1);
    }

    #[test]
    fn submitted_when_a_short_turn_already_ended() {
        let ssh = MockSsh::new(vec![probe_frame("idle", "")]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &SubmitPacing::zero());

        assert_eq!(result, Submission::Submitted);
    }

    #[test]
    fn submitted_when_pane_shows_claude_working() {
        let ssh = MockSsh::new(vec![probe_frame("pending", WORKING_PANE)]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &SubmitPacing::zero());

        assert_eq!(result, Submission::Submitted);
    }

    #[test]
    fn working_words_inside_the_stuck_input_box_are_not_proof() {
        let pane = "────────────────────────────────\n\
            > never press esc to interrupt the migration\n\
            ────────────────────────────────\n\
            \x20 ? for shortcuts\n";
        let pacing = SubmitPacing {
            max_enters: 1,
            ..SubmitPacing::zero()
        };
        let ssh = MockSsh::new(vec![probe_frame("pending", pane)]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &pacing);

        assert_eq!(result, Submission::NotSubmitted);
    }

    #[test]
    fn status_line_is_read_above_the_input_box_not_below_it() {
        let view = PaneView::parse(WORKING_PANE);

        assert!(view.shows_working());
        assert!(view.status.iter().all(|l| !l.contains("shortcuts")));
    }

    #[test]
    fn a_past_user_message_is_not_taken_for_the_input_box() {
        // `> fix the bug` in the transcript has no border above it.
        assert_eq!(
            find_input_box(&WORKING_PANE.lines().collect::<Vec<_>>()),
            Some((4, 5, 6))
        );
    }

    #[test]
    fn resends_enter_when_first_enter_was_swallowed() {
        let ssh = MockSsh::new(vec![
            probe_frame("pending", STUCK_PANE),
            ssh_ok(), // second Enter
            probe_frame("busy", ""),
        ]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &SubmitPacing::zero());

        assert_eq!(result, Submission::Submitted);
        let calls = ssh.calls();
        assert_eq!(calls[1], resend_enter_command("skulk-t"));
        assert!(calls[1].ends_with("tmux send-keys -t \"$skulk_win\" Enter"));
    }

    #[test]
    fn not_submitted_after_bounded_enters() {
        let ssh = MockSsh::new(vec![
            probe_frame("pending", STUCK_PANE),
            ssh_ok(),
            probe_frame("pending", STUCK_PANE),
            ssh_ok(),
            probe_frame("pending", STUCK_PANE),
        ]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &SubmitPacing::zero());

        assert_eq!(result, Submission::NotSubmitted);
        let enters = ssh.calls().iter().filter(|c| c.ends_with("Enter")).count();
        assert_eq!(enters, 2, "two resends on top of the first Enter");
    }

    #[test]
    fn polls_several_times_per_enter_before_resending() {
        let pacing = SubmitPacing {
            polls_per_enter: 3,
            ..SubmitPacing::zero()
        };
        let ssh = MockSsh::new(vec![
            probe_frame("pending", STUCK_PANE),
            probe_frame("pending", STUCK_PANE),
            probe_frame("busy", ""),
        ]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &pacing);

        assert_eq!(result, Submission::Submitted);
        assert!(ssh.calls().iter().all(|c| !c.ends_with("Enter")));
    }

    #[test]
    fn failed_probe_is_not_proof_of_submission() {
        let pacing = SubmitPacing {
            max_enters: 1,
            ..SubmitPacing::zero()
        };
        let ssh = MockSsh::new(vec![ssh_err("connection lost")]);

        let result = confirm_submission(&ssh, "skulk-t", &test_config(), &pacing);

        assert_eq!(result, Submission::NotSubmitted);
    }

    #[test]
    fn opencode_ignores_the_marker() {
        // OpenCode's plugin writes `idle` on its own schedule and nothing
        // writes `busy`, so only the pane can prove submission.
        let pacing = SubmitPacing {
            max_enters: 1,
            ..SubmitPacing::zero()
        };
        let ssh = MockSsh::new(vec![probe_frame("idle", STUCK_PANE)]);

        let result = confirm_submission(&ssh, "skulk-t", &opencode_config(), &pacing);

        assert_eq!(result, Submission::NotSubmitted);
    }

    #[test]
    fn opencode_submitted_when_pane_shows_it_working() {
        let ssh = MockSsh::new(vec![probe_frame("pending", "working…  esc interrupt")]);

        let result = confirm_submission(&ssh, "skulk-t", &opencode_config(), &SubmitPacing::zero());

        assert_eq!(result, Submission::Submitted);
    }
}
