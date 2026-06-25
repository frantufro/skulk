use std::collections::HashMap;

use crate::agent_ref::AgentRef;
use crate::config::{Config, OutputFormat};
use crate::display::{emit_json, format_sessions_table};
use crate::error::{SkulkError, is_tmux_no_server};
use crate::inventory::{
    AgentState, Session, get_worktree_map, parse_sessions, resolve_agent_state,
};
use crate::ssh::Ssh;
use crate::util::extract_section;

const TMUX_FORMAT: &str = "#{session_name}\t#{session_created}\t#{session_attached}";

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

/// Parse the state-file section into a map of session name -> marker contents.
///
/// Each line is `<session_name> <busy|idle>`. Lines that fail to parse are
/// silently ignored — state file infrastructure is delivered by the `wait`
/// task, and an absent / empty section simply means no idle data is available.
///
/// We read contents (not mtime) because `session_activity` does not tick
/// during Claude's extended-thinking redraws, so an mtime comparison would
/// misreport mid-thinking agents as idle.
fn parse_state_map(raw: &str) -> HashMap<String, String> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?.to_string();
            let content = parts.next()?.to_string();
            Some((name, content))
        })
        .collect()
}

/// Build the SSH command for `skulk list`: epoch, tmux sessions, worktrees,
/// and Stop/`UserPromptSubmit` hook marker contents in one call.
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
         for f in ~/.skulk/state/*; do [ -f \"$f\" ] && printf '%s %s\\n' \"$(basename \"$f\")\" \"$(cat \"$f\")\"; done 2>/dev/null; \
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
            session.state,
            state_map.get(&session.name).map(String::as_str),
        );
    }

    // Orphaned worktrees (no matching tmux session) appear as stopped agents
    for (name, path) in &worktree_map {
        if !sessions.iter().any(|s| &s.name == name) {
            sessions.push(Session {
                name: name.clone(),
                created: 0,
                state: AgentState::Stopped,
                worktree: Some(path.clone()),
            });
        }
    }

    (sessions, remote_now)
}

/// Build a JSON array of session objects for the `list` command's JSON output mode.
///
/// This is a pure function (no I/O) so it is directly unit-testable.
pub(crate) fn json_sessions(
    sessions: &[Session],
    remote_now: i64,
    cfg: &Config,
) -> serde_json::Value {
    let agents: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let agent_ref = AgentRef::from_qualified(&s.name, cfg);
            let uptime_secs: serde_json::Value = if s.state == AgentState::Stopped {
                serde_json::Value::Null
            } else {
                (remote_now - s.created).into()
            };
            let status = match s.state {
                AgentState::Attached => "attached",
                AgentState::Detached => "detached",
                AgentState::Idle => "idle",
                AgentState::Stopped => "stopped",
            };
            serde_json::json!({
                "name": agent_ref.name(),
                "status": status,
                "branch": agent_ref.branch_name(),
                "uptime_secs": uptime_secs,
                "session_created_epoch": s.created,
            })
        })
        .collect();
    serde_json::Value::Array(agents)
}

pub(crate) fn cmd_list(ssh: &impl Ssh, cfg: &Config) -> Result<(), SkulkError> {
    let output = ssh.run(&list_command(cfg))?;
    let (sessions, remote_now) = parse_list_output(&output, cfg);
    if cfg.output_format == OutputFormat::Json {
        emit_json(&json_sessions(&sessions, remote_now, cfg));
    } else {
        println!("{}", format_sessions_table(&sessions, remote_now, cfg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Session;
    use crate::testutil::{
        MockSsh, mock_list_output, mock_list_output_with_state, test_config, test_config_json,
    };

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
        let raw = "skulk-a idle\nskulk-b busy\n";
        let map = parse_state_map(raw);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("skulk-a").map(String::as_str), Some("idle"));
        assert_eq!(map.get("skulk-b").map(String::as_str), Some("busy"));
    }

    #[test]
    fn parse_state_map_skips_malformed_lines() {
        let raw = "skulk-a idle\nmalformed\nskulk-c busy\n";
        let map = parse_state_map(raw);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("skulk-a").map(String::as_str), Some("idle"));
        assert_eq!(map.get("skulk-c").map(String::as_str), Some("busy"));
    }

    #[test]
    fn parse_list_output_keeps_detached_state_without_state_file() {
        let cfg = test_config();
        let raw = mock_list_output(1_700_000_000, "skulk-test\t1700000000\t0", &[]);
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions[0].state, AgentState::Detached);
    }

    #[test]
    fn parse_list_output_marks_idle_when_marker_says_idle() {
        let cfg = test_config();
        let raw = mock_list_output_with_state(
            1_700_000_200,
            "skulk-done\t1700000000\t0",
            &[],
            &[("skulk-done", "idle")],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, AgentState::Idle);
    }

    #[test]
    fn parse_list_output_keeps_detached_when_marker_says_busy() {
        // Regression: previously `mtime >= activity` would flip mid-thinking
        // agents to Idle when `session_activity` froze during ANSI redraws.
        // Now the marker's literal "busy" content is the source of truth.
        let cfg = test_config();
        let raw = mock_list_output_with_state(
            1_700_000_200,
            "skulk-busy\t1700000000\t0",
            &[],
            &[("skulk-busy", "busy")],
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
            "skulk-test\t1700000000\t0",
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
            "skulk-one\t1700000000\t0\nother-session\t1700000000\t0",
            &[],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "skulk-one");
    }

    #[test]
    fn parse_remote_epoch_extracts_epoch() {
        let output = "__EPOCH__1700000000__EPOCH__\nskulk-test\t1700000000\t0";
        let epoch = parse_remote_epoch(output);
        assert_eq!(epoch, 1_700_000_000);
    }

    #[test]
    fn parse_remote_epoch_fallback_on_missing() {
        let output = "skulk-test\t1700000000\t0";
        let epoch = parse_remote_epoch(output);
        assert!(epoch > 0);
    }

    #[test]
    fn parse_remote_epoch_fallback_on_malformed() {
        let output = "__EPOCH__abc__EPOCH__\nskulk-test\t1700000000\t0";
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
            "skulk-test\t1700000000\t0",
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
            "skulk-test\t1700000000\t0",
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

    // ── JSON output mode ────────────────────────────────────────────────────

    fn make_session(
        name: &str,
        created: i64,
        state: AgentState,
        worktree: Option<&str>,
    ) -> Session {
        Session {
            name: name.to_string(),
            created,
            state,
            worktree: worktree.map(ToString::to_string),
        }
    }

    #[test]
    fn json_sessions_emits_array() {
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-my-task",
            1_700_000_000,
            AgentState::Detached,
            Some("/home/user/agents/skulk-my-task"),
        )];
        let value = json_sessions(&sessions, 1_700_000_200, &cfg);
        assert!(value.is_array());
        assert_eq!(value.as_array().unwrap().len(), 1);
    }

    #[test]
    fn json_sessions_includes_required_fields() {
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-my-task",
            1_700_000_000,
            AgentState::Detached,
            Some("/home/user/worktrees/skulk-my-task"),
        )];
        let value = json_sessions(&sessions, 1_700_000_200, &cfg);
        let obj = &value[0];
        assert_eq!(obj["name"], "my-task");
        assert_eq!(obj["status"], "detached");
        // branch is derived from the session name, not the worktree path
        assert_eq!(obj["branch"], "skulk-my-task");
        assert_eq!(obj["uptime_secs"], 200_i64);
        assert_eq!(obj["session_created_epoch"], 1_700_000_000_i64);
    }

    #[test]
    fn json_sessions_stopped_agent_has_null_uptime() {
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-zombie",
            0,
            AgentState::Stopped,
            Some("/wt/skulk-zombie"),
        )];
        let value = json_sessions(&sessions, 1_700_000_000, &cfg);
        let obj = &value[0];
        assert_eq!(obj["status"], "stopped");
        assert!(obj["uptime_secs"].is_null());
    }

    #[test]
    fn json_sessions_strips_session_prefix_from_name() {
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-feature-x",
            1_700_000_000,
            AgentState::Idle,
            None,
        )];
        let value = json_sessions(&sessions, 1_700_000_100, &cfg);
        assert_eq!(value[0]["name"], "feature-x");
    }

    #[test]
    fn json_sessions_branch_derived_from_session_name_when_no_worktree() {
        // branch is always the branch name from the session name, not the
        // worktree path — so it is never empty, even for stopped agents.
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-task",
            1_700_000_000,
            AgentState::Detached,
            None,
        )];
        let value = json_sessions(&sessions, 1_700_000_100, &cfg);
        assert_eq!(value[0]["branch"], "skulk-task");
    }

    #[test]
    fn cmd_list_json_mode_returns_ok() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![Ok(mock_list_output(
            1_700_000_000,
            "skulk-test\t1700000000\t0",
            &[("skulk-test", "/home/user/agents/skulk-test")],
        ))]);
        assert!(cmd_list(&ssh, &cfg).is_ok());
    }

    #[test]
    fn list_json_output_name_field_strips_session_prefix() {
        // The `name` field in JSON output must be the short agent name (without
        // the session prefix), matching what `AgentRef::name()` returns.
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-my-feature",
            1_700_000_000,
            AgentState::Detached,
            None,
        )];
        let value = json_sessions(&sessions, 1_700_000_100, &cfg);
        assert_eq!(
            value[0]["name"], "my-feature",
            "name field must strip session prefix"
        );
    }

    #[test]
    fn list_json_output_stopped_agent_has_null_uptime() {
        // Stopped agents have no running tmux session so uptime is undefined;
        // the JSON output must represent this as a JSON null, not a number.
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-dead-agent",
            0,
            AgentState::Stopped,
            Some("/wt/skulk-dead-agent"),
        )];
        let value = json_sessions(&sessions, 1_700_000_000, &cfg);
        let obj = &value[0];
        assert_eq!(obj["status"], "stopped");
        assert!(
            obj["uptime_secs"].is_null(),
            "uptime_secs must be null for stopped agents, got: {}",
            obj["uptime_secs"]
        );
    }
}
