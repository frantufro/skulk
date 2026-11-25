use std::collections::HashMap;

use crate::agent_ref::AgentRef;
use crate::config::{Config, OutputFormat};
use crate::display::{emit_json, format_sessions_table};
use crate::error::{SkulkError, is_tmux_no_server};
use crate::inventory::{
    AgentState, Session, parse_sessions, resolve_agent_state, worktree_map_from_output,
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

/// Parse the URL section into a map of worktree path -> app URL.
///
/// Each line is `<worktree path>\t<first line of url_file>`. Blank URLs are
/// dropped so an empty file reads the same as a missing one.
fn parse_url_map(raw: &str) -> HashMap<String, String> {
    raw.lines()
        .filter_map(|line| {
            let (path, url) = line.split_once('\t')?;
            let url = url.trim();
            (!url.is_empty()).then(|| (path.to_string(), url.to_string()))
        })
        .collect()
}

/// Build the SSH command for `skulk list`: the remote epoch followed by
/// [`agent_state_probe`].
pub(crate) fn list_command(cfg: &Config) -> String {
    format!(
        "echo __EPOCH__$(date +%s)__EPOCH__ && {}",
        agent_state_probe(cfg)
    )
}

/// Shell snippet printing tmux sessions, worktrees, Stop/`UserPromptSubmit`
/// hook marker contents, and each worktree's app URL (first line of
/// `url_file`) as delimited sections. Parse it with [`parse_agent_sessions`].
pub(crate) fn agent_state_probe(cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    let url_file = cfg.url_file();
    format!(
        "echo __TMUX_START__ && \
         tmux list-sessions -F '{TMUX_FORMAT}' 2>&1; \
         echo __TMUX_END__ && \
         echo __WORKTREES_START__ && \
         cd {base_path} && git worktree list --porcelain 2>/dev/null || true && \
         echo __WORKTREES_END__ && \
         echo __STATE_START__ && \
         for f in ~/.skulk/state/*; do [ -f \"$f\" ] && printf '%s %s\\n' \"$(basename \"$f\")\" \"$(cat \"$f\")\"; done 2>/dev/null; \
         echo __STATE_END__; \
         echo __URLS_START__; \
         cd {base_path} && git worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p' | \
         while IFS= read -r p; do [ -f \"$p/{url_file}\" ] && printf '%s\\t%s\\n' \"$p\" \"$(head -n 1 \"$p/{url_file}\")\"; done; \
         echo __URLS_END__"
    )
}

/// Parse the raw output of `list_command` into sessions and `remote_now` epoch.
pub(crate) fn parse_list_output(raw: &str, cfg: &Config) -> (Vec<Session>, i64) {
    (parse_agent_sessions(raw, cfg), parse_remote_epoch(raw))
}

/// Parse the output of [`agent_state_probe`] into one `Session` per agent:
/// live sessions with their idle state, plus worktrees without a session as
/// `Stopped`.
pub(crate) fn parse_agent_sessions(raw: &str, cfg: &Config) -> Vec<Session> {
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
    let worktree_map = worktree_map_from_output(raw, cfg);

    // Parse Stop-hook state files — optional; empty when the wait infrastructure
    // hasn't run yet.
    let state_raw = extract_section(raw, "__STATE_START__\n", "\n__STATE_END__");
    let state_map = parse_state_map(state_raw);

    let url_map = parse_url_map(extract_section(raw, "__URLS_START__\n", "\n__URLS_END__"));

    // Join worktree paths and upgrade live sessions to Idle when appropriate.
    for session in &mut sessions {
        session.worktree = worktree_map.get(&session.name).cloned();
        session.url = session
            .worktree
            .as_ref()
            .and_then(|path| url_map.get(path))
            .cloned();
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
                url: url_map.get(path).cloned(),
            });
        }
    }

    sessions
}

/// Build the `list` command's JSON payload: `{"agents": [...]}`.
///
/// The array sits inside an object so the payload can carry the `"v"` schema
/// version that `emit_json` stamps on every top-level object.
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
                "url": s.url,
            })
        })
        .collect();
    serde_json::json!({ "agents": agents })
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
        MockSsh, mock_list_output, mock_list_output_with_state, mock_urls_section, test_config,
        test_config_json,
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
            url: None,
        }
    }

    // ── url ─────────────────────────────────────────────────────────────────

    #[test]
    fn list_command_reads_default_url_file_in_each_worktree() {
        let cfg = test_config();

        let cmd = list_command(&cfg);

        assert!(cmd.contains("__URLS_START__"));
        assert!(cmd.contains("__URLS_END__"));
        assert!(cmd.contains("/tmp/dev-server-url"));
    }

    #[test]
    fn list_command_reads_configured_url_file() {
        let mut cfg = test_config();
        cfg.url_file = Some("log/app-url".into());

        let cmd = list_command(&cfg);

        assert!(cmd.contains("/log/app-url"));
        assert!(!cmd.contains("dev-server-url"));
    }

    #[test]
    fn parse_list_output_joins_url_by_worktree_path() {
        let cfg = test_config();
        let raw = mock_list_output(
            1_700_000_200,
            "skulk-a\t1700000000\t0\nskulk-b\t1700000000\t0",
            &[("skulk-a", "/wt/skulk-a"), ("skulk-b", "/wt/skulk-b")],
        ) + &mock_urls_section(&[("/wt/skulk-a", "  http://localhost:3042  ")]);

        let (sessions, _) = parse_list_output(&raw, &cfg);

        let url_of = |name: &str| {
            sessions
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.url.clone())
        };
        assert_eq!(url_of("skulk-a").as_deref(), Some("http://localhost:3042"));
        assert_eq!(url_of("skulk-b"), None);
    }

    #[test]
    fn parse_list_output_reports_url_for_stopped_agent_with_worktree() {
        let cfg = test_config();
        let raw = mock_list_output(1_700_000_200, "", &[("skulk-old", "/wt/skulk-old")])
            + &mock_urls_section(&[("/wt/skulk-old", "http://localhost:3050")]);

        let (sessions, _) = parse_list_output(&raw, &cfg);

        assert_eq!(sessions[0].url.as_deref(), Some("http://localhost:3050"));
    }

    #[test]
    fn parse_list_output_ignores_empty_url_file() {
        let cfg = test_config();
        let raw = mock_list_output(
            1_700_000_200,
            "skulk-a\t1700000000\t0",
            &[("skulk-a", "/wt/skulk-a")],
        ) + &mock_urls_section(&[("/wt/skulk-a", "")]);

        let (sessions, _) = parse_list_output(&raw, &cfg);

        assert!(sessions[0].url.is_none());
    }

    #[test]
    fn json_sessions_includes_url_and_null_when_missing() {
        let cfg = test_config_json();
        let mut with_url = make_session(
            "skulk-a",
            1_700_000_000,
            AgentState::Detached,
            Some("/wt/skulk-a"),
        );
        with_url.url = Some("http://localhost:3042".into());
        let without_url = make_session("skulk-b", 1_700_000_000, AgentState::Detached, None);

        let value = json_sessions(&[with_url, without_url], 1_700_000_200, &cfg);

        assert_eq!(value["agents"][0]["url"], "http://localhost:3042");
        assert_eq!(value["agents"][1]["url"], serde_json::Value::Null);
    }

    #[test]
    fn json_sessions_wraps_agents_in_an_object() {
        let cfg = test_config_json();
        let sessions = vec![make_session(
            "skulk-my-task",
            1_700_000_000,
            AgentState::Detached,
            Some("/home/user/agents/skulk-my-task"),
        )];
        let value = json_sessions(&sessions, 1_700_000_200, &cfg);
        assert!(value.is_object());
        assert_eq!(value["agents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn json_sessions_with_no_agents_has_empty_agents_array() {
        let cfg = test_config_json();

        let value = json_sessions(&[], 1_700_000_000, &cfg);

        assert_eq!(value["agents"], serde_json::json!([]));
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
        let obj = &value["agents"][0];
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
        let obj = &value["agents"][0];
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
        assert_eq!(value["agents"][0]["name"], "feature-x");
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
        assert_eq!(value["agents"][0]["branch"], "skulk-task");
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
            value["agents"][0]["name"], "my-feature",
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
        let obj = &value["agents"][0];
        assert_eq!(obj["status"], "stopped");
        assert!(
            obj["uptime_secs"].is_null(),
            "uptime_secs must be null for stopped agents, got: {}",
            obj["uptime_secs"]
        );
    }
}
