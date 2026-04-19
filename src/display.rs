use std::sync::atomic::{AtomicBool, Ordering};

use crate::agent_ref::AgentRef;
use crate::config::Config;
use crate::inventory::{AgentState, GcOrphans, Session};

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

pub(crate) static COLOR_ENABLED: AtomicBool = AtomicBool::new(true);

pub(crate) fn use_color() -> bool {
    COLOR_ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn checkmark(color: bool) -> &'static str {
    if color {
        "\x1b[32m\u{2713}\x1b[0m"
    } else {
        "[ok]"
    }
}

pub(crate) fn crossmark(color: bool) -> &'static str {
    if color {
        "\x1b[31m\u{2717}\x1b[0m"
    } else {
        "[FAIL]"
    }
}

pub(crate) fn warnmark(color: bool) -> &'static str {
    if color { "\x1b[33m!\x1b[0m" } else { "[warn]" }
}

pub(crate) fn bold(text: &str, color: bool) -> String {
    if color {
        format!("{BOLD}{text}{RESET}")
    } else {
        text.to_string()
    }
}

pub(crate) fn dim(text: &str, color: bool) -> String {
    if color {
        format!("{DIM}{text}{RESET}")
    } else {
        text.to_string()
    }
}

pub(crate) fn green(text: &str, color: bool) -> String {
    if color {
        format!("{GREEN}{text}{RESET}")
    } else {
        text.to_string()
    }
}

// ── Uptime / sessions table ─────────────────────────────────────────────────

pub(crate) fn format_uptime(remote_now: i64, created_epoch: i64) -> String {
    let secs = remote_now - created_epoch;

    if secs < 0 {
        return "just now".to_string();
    }

    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

pub(crate) fn format_sessions_table(sessions: &[Session], remote_now: i64, cfg: &Config) -> String {
    format_sessions_table_with_color(sessions, remote_now, use_color(), cfg)
}

pub(crate) fn format_sessions_table_with_color(
    sessions: &[Session],
    remote_now: i64,
    color: bool,
    cfg: &Config,
) -> String {
    if sessions.is_empty() {
        return "No agents running.\nUse `skulk new <name>` to create one.".to_string();
    }
    let mut lines = Vec::new();

    if color {
        lines.push(format!(
            "{BOLD}{:<20} {:<10} {:<12} {}{RESET}",
            "NAME", "STATUS", "UPTIME", "WORKTREE"
        ));
    } else {
        lines.push(format!(
            "{:<20} {:<10} {:<12} {}",
            "NAME", "STATUS", "UPTIME", "WORKTREE"
        ));
    }

    for s in sessions {
        let agent = AgentRef::from_qualified(&s.name, cfg);
        let display_name = agent.name();
        let status_raw = match s.state {
            AgentState::Attached => "attached",
            AgentState::Detached => "detached",
            AgentState::Idle => "idle",
            AgentState::Stopped => "stopped",
        };
        let status_padded = format!("{status_raw:<10}");
        let status_display = if color {
            match s.state {
                AgentState::Idle => format!("{GREEN}{BOLD}{status_padded}{RESET}"),
                AgentState::Attached | AgentState::Detached => {
                    format!("{GREEN}{status_padded}{RESET}")
                }
                AgentState::Stopped => format!("{YELLOW}{status_padded}{RESET}"),
            }
        } else {
            status_padded
        };
        let uptime = if s.state == AgentState::Stopped {
            "-".to_string()
        } else {
            format_uptime(remote_now, s.created)
        };
        let wt = s.worktree.as_deref().unwrap_or("-");
        lines.push(format!(
            "{display_name:<20} {status_display} {uptime:<12} {wt}"
        ));
    }

    lines.join("\n")
}

// ── GC display ──────────────────────────────────────────────────────────────

/// Format a gc summary for display.
pub(crate) fn format_gc_summary(orphans: &GcOrphans, dry_run: bool) -> String {
    if orphans.is_empty() {
        return "No orphaned resources found. Everything is clean.".to_string();
    }

    let action = if dry_run { "Would clean" } else { "Cleaned" };
    let mut lines = Vec::new();

    if !orphans.sessions.is_empty() {
        lines.push(format!(
            "{action} {} orphaned tmux session(s):",
            orphans.sessions.len()
        ));
        for s in &orphans.sessions {
            lines.push(format!("  - {s}"));
        }
    }

    if !orphans.worktrees.is_empty() {
        lines.push(format!(
            "{action} {} orphaned worktree(s):",
            orphans.worktrees.len()
        ));
        for w in &orphans.worktrees {
            lines.push(format!("  - {w}"));
        }
    }

    if !orphans.branches.is_empty() {
        lines.push(format!(
            "{action} {} orphaned branch(es):",
            orphans.branches.len()
        ));
        for b in &orphans.branches {
            lines.push(format!("  - {b}"));
        }
    }

    lines.push(String::new());
    lines.push(format!("Total: {} orphaned resource(s).", orphans.total()));

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_config;

    fn sess(name: &str, created: i64, state: AgentState, worktree: Option<&str>) -> Session {
        Session {
            name: name.to_string(),
            created,
            activity: created,
            state,
            worktree: worktree.map(ToString::to_string),
        }
    }

    // ── format_uptime ───────────────────────────────────────────────────

    #[test]
    fn format_uptime_minutes() {
        assert_eq!(format_uptime(1000090, 1000000), "1m");
    }

    #[test]
    fn format_uptime_hours_and_minutes() {
        assert_eq!(format_uptime(1003700, 1000000), "1h 1m");
    }

    #[test]
    fn format_uptime_days_and_hours() {
        assert_eq!(format_uptime(1090000, 1000000), "1d 1h");
    }

    #[test]
    fn format_uptime_negative_returns_just_now() {
        assert_eq!(format_uptime(1000000, 1000100), "just now");
    }

    #[test]
    fn format_uptime_zero_returns_0m() {
        assert_eq!(format_uptime(1000000, 1000000), "0m");
    }

    // ── format_sessions_table ───────────────────────────────────────────

    #[test]
    fn format_sessions_table_empty() {
        let cfg = test_config();
        let result = format_sessions_table(&[], 1000000, &cfg);
        assert_eq!(
            result,
            "No agents running.\nUse `skulk new <name>` to create one."
        );
    }

    #[test]
    fn format_sessions_table_has_header() {
        let cfg = test_config();
        let sessions = vec![sess("skulk-test", 1000000, AgentState::Detached, None)];
        let result = format_sessions_table(&sessions, 1000090, &cfg);
        let first_line = result.lines().next().unwrap();
        assert!(first_line.contains("NAME"));
        assert!(first_line.contains("STATUS"));
        assert!(first_line.contains("UPTIME"));
        assert!(first_line.contains("WORKTREE"));
        assert!(result.contains("test"));
        assert!(!result.lines().nth(1).unwrap().starts_with("skulk-"));
    }

    #[test]
    fn format_sessions_table_attached_status() {
        let cfg = test_config();
        let sessions = vec![sess("skulk-attached", 1000000, AgentState::Attached, None)];
        let result = format_sessions_table(&sessions, 1000090, &cfg);
        assert!(result.contains("attached"));
    }

    #[test]
    fn format_sessions_table_detached_status() {
        let cfg = test_config();
        let sessions = vec![sess("skulk-detached", 1000000, AgentState::Detached, None)];
        let result = format_sessions_table(&sessions, 1000090, &cfg);
        assert!(result.contains("detached"));
    }

    #[test]
    fn format_sessions_table_worktree_placeholder() {
        let cfg = test_config();
        let sessions = vec![sess("skulk-wt", 1000000, AgentState::Detached, None)];
        let result = format_sessions_table(&sessions, 1000090, &cfg);
        let data_lines: Vec<&str> = result.lines().skip(1).collect();
        assert!(!data_lines.is_empty());
        for line in data_lines {
            assert!(line.contains('-'));
        }
    }

    #[test]
    fn format_sessions_table_with_worktree_path() {
        let cfg = test_config();
        let sessions = vec![sess(
            "skulk-test",
            1000000,
            AgentState::Detached,
            Some("~/test-project-worktrees/skulk-test"),
        )];
        let result = format_sessions_table(&sessions, 1000090, &cfg);
        assert!(result.contains("~/test-project-worktrees/skulk-test"));
    }

    #[test]
    fn format_sessions_table_strips_agent_prefix_from_name() {
        let cfg = test_config();
        let sessions = vec![sess("skulk-my-task", 1000000, AgentState::Detached, None)];
        let result = format_sessions_table_with_color(&sessions, 1000090, false, &cfg);
        let data_line = result.lines().nth(1).unwrap();
        assert!(data_line.starts_with("my-task"));
        assert!(!data_line.starts_with("skulk-"));
    }

    #[test]
    fn format_sessions_table_stopped_status() {
        let cfg = test_config();
        let sessions = vec![sess(
            "skulk-zombie",
            0,
            AgentState::Stopped,
            Some("~/test-project-worktrees/skulk-zombie"),
        )];
        let result = format_sessions_table_with_color(&sessions, 1000090, false, &cfg);
        assert!(
            result.contains("stopped"),
            "should show stopped status: {result}"
        );
    }

    #[test]
    fn format_sessions_table_stopped_shows_dash_for_uptime() {
        let cfg = test_config();
        let sessions = vec![sess(
            "skulk-zombie",
            0,
            AgentState::Stopped,
            Some("~/test-project-worktrees/skulk-zombie"),
        )];
        let result = format_sessions_table_with_color(&sessions, 1000090, false, &cfg);
        let data_line = result.lines().nth(1).unwrap();
        assert!(
            data_line.contains("stopped"),
            "should show stopped: {data_line}"
        );
        let parts: Vec<&str> = data_line.split_whitespace().collect();
        // name, status, uptime, worktree...
        assert_eq!(parts[1], "stopped");
        assert_eq!(parts[2], "-");
    }

    #[test]
    fn format_sessions_table_shows_idle_state_value() {
        let cfg = test_config();
        let working = sess("skulk-busy", 1000000, AgentState::Detached, None);
        let idle_agent = sess("skulk-done", 1000000, AgentState::Idle, None);
        let result = format_sessions_table_with_color(&[working, idle_agent], 1000090, false, &cfg);
        assert!(result.contains("detached"));
        assert!(result.contains("idle"));
    }

    #[test]
    fn format_sessions_table_idle_highlighted_in_color() {
        let cfg = test_config();
        let s = sess("skulk-done", 1000000, AgentState::Idle, None);
        let output = format_sessions_table_with_color(&[s], 1000090, true, &cfg);
        // Idle value should appear in bold green; the raw word "idle" is present.
        assert!(output.contains("idle"));
        assert!(output.contains("\x1b[32m"));
        assert!(output.contains("\x1b[1m"));
    }

    #[test]
    fn format_sessions_table_contains_color_when_enabled() {
        let cfg = test_config();
        let sessions = vec![sess("skulk-test", 1700000000, AgentState::Detached, None)];
        let output = format_sessions_table_with_color(&sessions, 1700000200, true, &cfg);
        assert!(output.contains("\x1b[32m"));
        assert!(output.contains("\x1b[0m"));
        assert!(output.contains("\x1b[1m"));
    }

    #[test]
    fn format_sessions_table_no_color_when_disabled() {
        let cfg = test_config();
        let sessions = vec![sess("skulk-test", 1700000000, AgentState::Detached, None)];
        let output = format_sessions_table_with_color(&sessions, 1700000200, false, &cfg);
        assert!(!output.contains("\x1b["));
    }

    // ── format_gc_summary ───────────────────────────────────────────────

    #[test]
    fn format_gc_summary_clean() {
        let orphans = GcOrphans {
            sessions: vec![],
            worktrees: vec![],
            branches: vec![],
        };
        let summary = format_gc_summary(&orphans, false);
        assert!(summary.contains("clean"));
    }

    #[test]
    fn format_gc_summary_dry_run() {
        let orphans = GcOrphans {
            sessions: vec!["skulk-ghost".into()],
            worktrees: vec![],
            branches: vec![],
        };
        let summary = format_gc_summary(&orphans, true);
        assert!(summary.contains("Would clean"));
    }

    #[test]
    fn format_gc_summary_actual_run() {
        let orphans = GcOrphans {
            sessions: vec!["skulk-ghost".into()],
            worktrees: vec![],
            branches: vec![],
        };
        let summary = format_gc_summary(&orphans, false);
        assert!(summary.contains("Cleaned"));
        assert!(summary.contains("skulk-ghost"));
    }

    #[test]
    fn format_gc_summary_shows_total() {
        let orphans = GcOrphans {
            sessions: vec!["a".into()],
            worktrees: vec!["b".into()],
            branches: vec!["c".into()],
        };
        let summary = format_gc_summary(&orphans, false);
        assert!(summary.contains("Total: 3"));
    }

    #[test]
    fn format_gc_summary_dry_run_all_types() {
        let orphans = GcOrphans {
            sessions: vec!["skulk-sess".into()],
            worktrees: vec!["skulk-wt".into()],
            branches: vec!["skulk-br".into()],
        };
        let summary = format_gc_summary(&orphans, true);
        assert!(summary.contains("Would clean"));
        assert!(summary.contains("skulk-sess"));
        assert!(summary.contains("skulk-wt"));
        assert!(summary.contains("skulk-br"));
        assert!(summary.contains("Total: 3"));
        assert!(!summary.contains("Cleaned"));
    }

    #[test]
    fn format_gc_summary_actual_all_types() {
        let orphans = GcOrphans {
            sessions: vec!["skulk-sess".into()],
            worktrees: vec!["skulk-wt".into()],
            branches: vec!["skulk-br".into()],
        };
        let summary = format_gc_summary(&orphans, false);
        assert!(summary.contains("Cleaned"));
        assert!(!summary.contains("Would clean"));
    }

    #[test]
    fn format_gc_summary_empty_is_same_for_dry_run() {
        let orphans = GcOrphans {
            sessions: vec![],
            worktrees: vec![],
            branches: vec![],
        };
        let dry = format_gc_summary(&orphans, true);
        let actual = format_gc_summary(&orphans, false);
        assert_eq!(dry, actual);
    }
}
