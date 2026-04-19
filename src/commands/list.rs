use std::collections::HashMap;

use crate::config::Config;
use crate::display::format_sessions_table;
use crate::error::{SkulkError, is_tmux_no_server};
use crate::inventory::{
    AgentState, Session, get_worktree_map, parse_sessions, resolve_agent_state,
};
use crate::ssh::Ssh;
use crate::util::extract_section;

const TMUX_FORMAT: &str =
    "#{session_name}\t#{session_created}\t#{session_activity}\t#{session_attached}";

fn parse_remote_epoch(output: &str) -> i64 {
    output
        .lines()
        .find(|l| l.contains("__EPOCH__"))
        .and_then(|l| l.replace("__EPOCH__", "").trim().parse::<i64>().ok())
        .unwrap_or_else(|| {
            eprintln!("Warning: could not parse remote clock; using local time.");
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs().cast_signed())
        })
}

/// Parse the state-file section into a map of session name -> Stop-hook mtime.
///
/// Each line is `<session_name> <unix_seconds>`. Lines that fail to parse are
/// silently ignored — state file infrastructure is delivered by the `wait`
/// task, and an absent / empty section simply means no idle data is available.
fn parse_state_map(raw: &str) -> HashMap<String, i64> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?.to_string();
            let mtime = parts.next()?.parse::<i64>().ok()?;
            Some((name, mtime))
        })
        .collect()
}

/// Build the SSH command for `skulk list`: epoch, tmux sessions, worktrees,
/// and Stop-hook state files in one call.
pub(crate) fn list_command(cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    format!(
        "echo __EPOCH__$(date +%s)__EPOCH__ && \
         echo __TMUX_START__ && \
         tmux list-sessions -F '{TMUX_FORMAT}' 2>&1 && \
         echo __TMUX_END__ && \
         echo __WORKTREES_START__ && \
         cd {base_path} && git worktree list --porcelain 2>/dev/null || true && \
         echo __WORKTREES_END__ && \
         echo __STATE_START__ && \
         for f in ~/.skulk/state/*; do [ -f \"$f\" ] && printf '%s %s\\n' \"$(basename \"$f\")\" \"$(stat -c %Y \"$f\")\"; done 2>/dev/null; \
         echo __STATE_END__"
    )
}

/// Parse the raw output of `list_command` into sessions and `remote_now` epoch.
pub(crate) fn parse_list_output(raw: &str, cfg: &Config) -> (Vec<Session>, i64) {
    let remote_now = parse_remote_epoch(raw);

    // Parse tmux sessions
    let tmux_raw = extract_section(raw, "__TMUX_START__\n", "\n__TMUX_END__");
    let tmux_output = if tmux_raw.lines().any(is_tmux_no_server) {
        ""
    } else {
        tmux_raw
    };
    let mut sessions: Vec<Session> = parse_sessions(tmux_output)
        .into_iter()
        .filter(|s| s.name.starts_with(&*cfg.session_prefix))
        .collect();

    // Parse worktree map
    let worktrees_raw = extract_section(raw, "__WORKTREES_START__\n", "\n__WORKTREES_END__");
    let worktree_map = get_worktree_map(worktrees_raw, cfg);

    // Parse Stop-hook state files — optional; empty when the wait infrastructure
    // hasn't run yet.
    let state_raw = extract_section(raw, "__STATE_START__\n", "\n__STATE_END__");
    let state_map = parse_state_map(state_raw);

    // Join worktree paths and upgrade live sessions to Idle when appropriate.
    for session in &mut sessions {
        session.worktree = worktree_map.get(&session.name).cloned();
        session.state = resolve_agent_state(
            &session.state,
            session.activity,
            state_map.get(&session.name).copied(),
        );
    }

    // Orphaned worktrees (no matching tmux session) appear as stopped agents
    for (name, path) in &worktree_map {
        if !sessions.iter().any(|s| &s.name == name) {
            sessions.push(Session {
                name: name.clone(),
                created: 0,
                activity: 0,
                state: AgentState::Stopped,
                worktree: Some(path.clone()),
            });
        }
    }

    (sessions, remote_now)
}

pub(crate) fn cmd_list(ssh: &impl Ssh, cfg: &Config) -> Result<(), SkulkError> {
    let output = ssh.run(&list_command(cfg))?;
    let (sessions, remote_now) = parse_list_output(&output, cfg);
    println!("{}", format_sessions_table(&sessions, remote_now, cfg));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, mock_list_output, mock_list_output_with_state, test_config};

    #[test]
    fn list_command_contains_epoch_and_tmux_and_worktree_markers() {
        let cfg = test_config();
        let cmd = list_command(&cfg);
        assert!(cmd.contains("__EPOCH__"));
        assert!(cmd.contains("__TMUX_START__"));
        assert!(cmd.contains("__TMUX_END__"));
        assert!(cmd.contains("__WORKTREES_START__"));
        assert!(cmd.contains("__WORKTREES_END__"));
        assert!(cmd.contains("__STATE_START__"));
        assert!(cmd.contains("__STATE_END__"));
        assert!(cmd.contains("~/.skulk/state/"));
    }

    #[test]
    fn parse_state_map_parses_lines() {
        let raw = "skulk-a 1700000000\nskulk-b 1700000050\n";
        let map = parse_state_map(raw);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("skulk-a"), Some(&1_700_000_000));
        assert_eq!(map.get("skulk-b"), Some(&1_700_000_050));
    }

    #[test]
    fn parse_state_map_skips_malformed_lines() {
        let raw = "skulk-a 1700000000\nmalformed\nskulk-b not_a_number\nskulk-c 42\n";
        let map = parse_state_map(raw);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("skulk-a"), Some(&1_700_000_000));
        assert_eq!(map.get("skulk-c"), Some(&42));
    }

    #[test]
    fn parse_list_output_keeps_detached_state_without_state_file() {
        let cfg = test_config();
        let raw = mock_list_output(1_700_000_000, "skulk-test\t1700000000\t1700000100\t0", &[]);
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions[0].state, AgentState::Detached);
    }

    #[test]
    fn parse_list_output_marks_idle_when_state_mtime_ge_activity() {
        let cfg = test_config();
        let raw = mock_list_output_with_state(
            1_700_000_200,
            "skulk-done\t1700000000\t1700000100\t0",
            &[],
            &[("skulk-done", 1_700_000_100)],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, AgentState::Idle);
    }

    #[test]
    fn parse_list_output_keeps_detached_when_activity_after_state_mtime() {
        let cfg = test_config();
        let raw = mock_list_output_with_state(
            1_700_000_200,
            "skulk-busy\t1700000000\t1700000150\t0",
            &[],
            &[("skulk-busy", 1_700_000_100)],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, AgentState::Detached);
    }

    #[test]
    fn parse_list_output_stopped_session_stays_stopped() {
        let cfg = test_config();
        let raw = mock_list_output(
            1_700_000_000,
            "no server running on /tmp/tmux-1000/default",
            &[("skulk-zombie", "/home/user/agents/skulk-zombie")],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions[0].state, AgentState::Stopped);
    }

    #[test]
    fn parse_list_output_extracts_sessions_and_worktrees() {
        let cfg = test_config();
        let raw = mock_list_output(
            1_700_000_000,
            "skulk-test\t1700000000\t1700000100\t0",
            &[("skulk-test", "/home/user/agents/skulk-test")],
        );
        let (sessions, remote_now) = parse_list_output(&raw, &cfg);
        assert_eq!(remote_now, 1_700_000_000);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "skulk-test");
        assert_eq!(
            sessions[0].worktree.as_deref(),
            Some("/home/user/agents/skulk-test")
        );
    }

    #[test]
    fn parse_list_output_handles_no_server() {
        let cfg = test_config();
        let raw = mock_list_output(
            1_700_000_000,
            "no server running on /tmp/tmux-1000/default",
            &[],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert!(sessions.is_empty());
    }

    #[test]
    fn parse_list_output_filters_non_agent_sessions() {
        let cfg = test_config();
        let raw = mock_list_output(
            1_700_000_000,
            "skulk-one\t1700000000\t1700000100\t0\nother-session\t1700000000\t1700000100\t0",
            &[],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "skulk-one");
    }

    #[test]
    fn parse_remote_epoch_extracts_epoch() {
        let output = "__EPOCH__1700000000__EPOCH__\nskulk-test\t1700000000\t1700000100\t0";
        let epoch = parse_remote_epoch(output);
        assert_eq!(epoch, 1700000000);
    }

    #[test]
    fn parse_remote_epoch_fallback_on_missing() {
        let output = "skulk-test\t1700000000\t1700000100\t0";
        let epoch = parse_remote_epoch(output);
        assert!(epoch > 0);
    }

    #[test]
    fn parse_remote_epoch_fallback_on_malformed() {
        let output = "__EPOCH__abc__EPOCH__\nskulk-test\t1700000000\t1700000100\t0";
        let epoch = parse_remote_epoch(output);
        assert!(epoch > 0);
    }

    #[test]
    fn parse_list_output_shows_orphaned_worktree_as_stopped() {
        let cfg = test_config();
        // Worktree exists but no matching tmux session
        let raw = mock_list_output(
            1_700_000_000,
            "no server running on /tmp/tmux-1000/default",
            &[("skulk-zombie", "/home/user/agents/skulk-zombie")],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "skulk-zombie");
        assert_eq!(sessions[0].state, AgentState::Stopped);
        assert_eq!(
            sessions[0].worktree.as_deref(),
            Some("/home/user/agents/skulk-zombie")
        );
    }

    #[test]
    fn parse_list_output_does_not_duplicate_matched_worktrees() {
        let cfg = test_config();
        // Session AND worktree both exist — should appear once as Detached, not also as Stopped
        let raw = mock_list_output(
            1_700_000_000,
            "skulk-test\t1700000000\t1700000100\t0",
            &[("skulk-test", "/home/user/agents/skulk-test")],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, AgentState::Detached);
    }

    #[test]
    fn cmd_list_returns_ok_with_sessions() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_list_output(
            1_700_000_000,
            "skulk-test\t1700000000\t1700000100\t0",
            &[("skulk-test", "/home/user/agents/skulk-test")],
        ))]);
        assert!(cmd_list(&ssh, &cfg).is_ok());
    }

    #[test]
    fn cmd_list_handles_no_server_running() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_list_output(
            1_700_000_000,
            "no server running on /tmp/tmux-1000/default",
            &[],
        ))]);
        assert!(cmd_list(&ssh, &cfg).is_ok());
    }
}
