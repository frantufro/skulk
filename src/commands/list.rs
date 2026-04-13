use crate::config::Config;
use crate::display::{Session, SessionState, format_sessions_table, parse_sessions};
use crate::error::{SkulkError, is_tmux_no_server};
use crate::inventory::get_worktree_map;
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

/// Build the SSH command for `skulk list`: epoch, full tmux sessions, and worktrees in one call.
pub(crate) fn list_command(cfg: &Config) -> String {
    let base_path = &cfg.base_path;
    format!(
        "echo __EPOCH__$(date +%s)__EPOCH__ && \
         echo __TMUX_START__ && \
         tmux list-sessions -F '{TMUX_FORMAT}' 2>&1 && \
         echo __TMUX_END__ && \
         echo __WORKTREES_START__ && \
         cd {base_path} && git worktree list --porcelain 2>/dev/null || true && \
         echo __WORKTREES_END__"
    )
}

/// Parse the raw output of `list_command` into sessions and `remote_now` epoch.
pub(crate) fn parse_list_output(raw: &str, cfg: &Config) -> (Vec<Session>, i64) {
    let remote_now = parse_remote_epoch(raw);

    // Parse tmux sessions
    let tmux_raw = extract_section(raw, "__TMUX_START__\n", "\n__TMUX_END__");
    let tmux_output = if tmux_raw.lines().any(is_tmux_no_server) {
        String::new()
    } else {
        tmux_raw
    };
    let mut sessions: Vec<Session> = parse_sessions(&tmux_output)
        .into_iter()
        .filter(|s| s.name.starts_with(&*cfg.session_prefix))
        .collect();

    // Parse worktree map
    let worktrees_raw = extract_section(raw, "__WORKTREES_START__\n", "\n__WORKTREES_END__");
    let worktree_map = get_worktree_map(&worktrees_raw, cfg);

    // Join worktree paths with session data
    for session in &mut sessions {
        session.worktree = worktree_map.get(&session.name).cloned();
    }

    // Orphaned worktrees (no matching tmux session) appear as stopped agents
    for (name, path) in &worktree_map {
        if !sessions.iter().any(|s| &s.name == name) {
            sessions.push(Session {
                name: name.clone(),
                created: 0,
                state: SessionState::Stopped,
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
    use crate::display::SessionState;
    use crate::testutil::{MockSsh, mock_list_output, test_config};

    #[test]
    fn list_command_contains_epoch_and_tmux_and_worktree_markers() {
        let cfg = test_config();
        let cmd = list_command(&cfg);
        assert!(cmd.contains("__EPOCH__"));
        assert!(cmd.contains("__TMUX_START__"));
        assert!(cmd.contains("__TMUX_END__"));
        assert!(cmd.contains("__WORKTREES_START__"));
        assert!(cmd.contains("__WORKTREES_END__"));
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
        assert_eq!(sessions[0].state, SessionState::Stopped);
        assert_eq!(
            sessions[0].worktree.as_deref(),
            Some("/home/user/agents/skulk-zombie")
        );
    }

    #[test]
    fn parse_list_output_does_not_duplicate_matched_worktrees() {
        let cfg = test_config();
        // Session AND worktree both exist — should appear once as Running, not also as Stopped
        let raw = mock_list_output(
            1_700_000_000,
            "skulk-test\t1700000000\t1700000100\t0",
            &[("skulk-test", "/home/user/agents/skulk-test")],
        );
        let (sessions, _) = parse_list_output(&raw, &cfg);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, SessionState::Running);
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
