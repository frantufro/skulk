use crate::agent_ref::AgentRef;
use crate::config::Config;
use crate::display::{format_uptime, use_color};
use crate::error::{SkulkError, classify_agent_error, is_tmux_no_server};
use crate::inventory::{AgentState, get_worktree_map, parse_sessions, resolve_agent_state};
use crate::ssh::Ssh;
use crate::util::{extract_section, validate_name};

const TMUX_FORMAT: &str =
    "#{session_name}\t#{session_created}\t#{session_activity}\t#{session_attached}";

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Parsed view of a single agent, ready for rendering.
#[derive(Debug)]
pub(crate) struct StatusView {
    pub display_name: String,
    pub state: AgentState,
    /// `None` when the session is stopped — there's no tmux `session_created`
    /// to anchor an uptime to.
    pub uptime: Option<String>,
    pub branch: String,
    pub default_branch: String,
    pub commits_ahead: u32,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
    pub worktree: Option<String>,
}

/// Parse `__EPOCH__<n>__EPOCH__` and fall back to local clock on miss.
///
/// Duplicated from `list.rs` rather than lifted into a shared helper: keeping
/// `status` self-contained means a structural refactor isn't entangled with
/// this feature commit.
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

/// Parse the `tail -1` line of `git diff --stat` into `(files, insertions, deletions)`.
///
/// Real output shapes:
/// - `" 5 files changed, 120 insertions(+), 34 deletions(-)"`
/// - `" 1 file changed, 10 insertions(+)"` (singular, no deletions)
/// - `" 2 files changed, 3 deletions(-)"` (no insertions)
/// - `""` (no changes) → all zeros
pub(crate) fn parse_diff_stat(line: &str) -> (u32, u32, u32) {
    let mut files = 0;
    let mut insertions = 0;
    let mut deletions = 0;
    for part in line.split(',') {
        let part = part.trim();
        let Some((num_str, rest)) = part.split_once(' ') else {
            continue;
        };
        let Ok(n) = num_str.parse::<u32>() else {
            continue;
        };
        if rest.contains("insertion") {
            insertions = n;
        } else if rest.contains("deletion") {
            deletions = n;
        } else if rest.contains("file") {
            files = n;
        }
    }
    (files, insertions, deletions)
}

/// Build the single SSH roundtrip that gathers everything `skulk status` needs:
/// remote epoch, tmux sessions, worktree porcelain, branch existence, Stop-hook
/// state mtime, commits ahead, and `git diff --stat` summary.
pub(crate) fn status_command(name: &str, cfg: &Config) -> String {
    let base = &cfg.base_path;
    let default = &cfg.default_branch;
    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let branch = agent.branch_name();
    format!(
        "echo __EPOCH__$(date +%s)__EPOCH__ && \
         echo __TMUX_START__ && \
         tmux list-sessions -F '{TMUX_FORMAT}' 2>&1 || true && \
         echo __TMUX_END__ && \
         echo __WORKTREES_START__ && \
         cd {base} && git worktree list --porcelain 2>/dev/null || true && \
         echo __WORKTREES_END__ && \
         echo __BRANCH_EXISTS_START__ && \
         cd {base} && (git show-ref --verify --quiet refs/heads/{branch} && echo yes || echo no) && \
         echo __BRANCH_EXISTS_END__ && \
         echo __STATE_START__ && \
         {{ [ -f ~/.skulk/state/{session_name} ] && stat -c %Y ~/.skulk/state/{session_name}; }} 2>/dev/null; \
         echo __STATE_END__ && \
         echo __REVCOUNT_START__ && \
         cd {base} && git rev-list --count {default}..{branch} 2>/dev/null || true && \
         echo __REVCOUNT_END__ && \
         echo __DIFFSTAT_START__ && \
         cd {base} && (git diff --stat {default}...{branch} 2>/dev/null | tail -1) || true && \
         echo __DIFFSTAT_END__"
    )
}

/// Parse the raw output of [`status_command`] into a [`StatusView`].
///
/// Returns `NotFound` when none of session / worktree / branch exist on the
/// remote — a deliberately strict definition of "agent exists" so scripted
/// callers get a clean nonzero exit when they ask about a missing name.
pub(crate) fn parse_status_output(
    raw: &str,
    name: &str,
    cfg: &Config,
) -> Result<StatusView, SkulkError> {
    let agent = AgentRef::new(name, cfg);
    let session_name = agent.session_name();
    let branch = agent.branch_name();

    let remote_now = parse_remote_epoch(raw);

    let tmux_raw = extract_section(raw, "__TMUX_START__\n", "\n__TMUX_END__");
    let tmux_output = if tmux_raw.lines().any(is_tmux_no_server) {
        ""
    } else {
        tmux_raw
    };
    let our_session = parse_sessions(tmux_output)
        .into_iter()
        .find(|s| s.name == session_name);

    let wt_raw = extract_section(raw, "__WORKTREES_START__\n", "\n__WORKTREES_END__");
    let worktree = get_worktree_map(wt_raw, cfg).get(&session_name).cloned();

    let be_raw = extract_section(raw, "__BRANCH_EXISTS_START__\n", "\n__BRANCH_EXISTS_END__");
    let branch_exists = be_raw.lines().any(|l| l.trim() == "yes");

    if our_session.is_none() && worktree.is_none() && !branch_exists {
        return Err(SkulkError::NotFound(format!(
            "Agent '{name}' not found. Check running agents with `skulk list`."
        )));
    }

    let state_raw = extract_section(raw, "__STATE_START__\n", "\n__STATE_END__");
    let state_mtime = state_raw
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<i64>().ok());

    let rc_raw = extract_section(raw, "__REVCOUNT_START__\n", "\n__REVCOUNT_END__");
    let commits_ahead = rc_raw
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u32>().ok())
        .unwrap_or(0);

    let ds_raw = extract_section(raw, "__DIFFSTAT_START__\n", "\n__DIFFSTAT_END__");
    let diffstat_line = ds_raw.lines().last().unwrap_or("").trim();
    let (files_changed, insertions, deletions) = parse_diff_stat(diffstat_line);

    let (state, uptime) = match &our_session {
        Some(s) => {
            let state = resolve_agent_state(s.state, s.activity, state_mtime);
            (state, Some(format_uptime(remote_now, s.created)))
        }
        None => (AgentState::Stopped, None),
    };

    Ok(StatusView {
        display_name: agent.name().to_string(),
        state,
        uptime,
        branch,
        default_branch: cfg.default_branch.clone(),
        commits_ahead,
        files_changed,
        insertions,
        deletions,
        worktree,
    })
}

pub(crate) fn format_status(view: &StatusView) -> String {
    format_status_with_color(view, use_color())
}

/// Palette mirrors `format_sessions_table_with_color` in `display.rs`:
/// idle = bold green (highlighted because it's waiting for you), attached /
/// detached = green, stopped = yellow. Not parameterised via `display.rs`
/// helpers because status keys colour off `AgentState` directly, so reusing
/// `green()` / `bold()` would be a net wash.
pub(crate) fn format_status_with_color(view: &StatusView, color: bool) -> String {
    let status_raw = match view.state {
        AgentState::Attached => "attached",
        AgentState::Detached => "detached",
        AgentState::Idle => "idle",
        AgentState::Stopped => "stopped",
    };
    let status_display = if color {
        match view.state {
            AgentState::Idle => format!("{GREEN}{BOLD}{status_raw}{RESET}"),
            AgentState::Attached | AgentState::Detached => {
                format!("{GREEN}{status_raw}{RESET}")
            }
            AgentState::Stopped => format!("{YELLOW}{status_raw}{RESET}"),
        }
    } else {
        status_raw.to_string()
    };

    let uptime = view.uptime.as_deref().unwrap_or("-");
    let files = if view.files_changed == 0 {
        "0 changed".to_string()
    } else {
        format!(
            "{} changed (+{} -{})",
            view.files_changed, view.insertions, view.deletions
        )
    };
    let worktree = view.worktree.as_deref().unwrap_or("-");

    format!(
        "Agent:    {name}\n\
         Status:   {status_display}\n\
         Uptime:   {uptime}\n\
         Branch:   {branch}\n\
         Commits:  {commits} ahead of {default}\n\
         Files:    {files}\n\
         Worktree: {worktree}",
        name = view.display_name,
        branch = view.branch,
        commits = view.commits_ahead,
        default = view.default_branch,
    )
}

pub(crate) fn cmd_status(ssh: &impl Ssh, name: &str, cfg: &Config) -> Result<(), SkulkError> {
    validate_name(name)?;
    let raw = ssh
        .run(&status_command(name, cfg))
        .map_err(|e| classify_agent_error(name, e, &cfg.host))?;
    let view = parse_status_output(&raw, name, cfg)?;
    println!("{}", format_status(&view));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, mock_status_output, test_config};

    // ── parse_diff_stat ─────────────────────────────────────────────────

    #[test]
    fn parse_diff_stat_full_line() {
        let (f, i, d) = parse_diff_stat(" 5 files changed, 120 insertions(+), 34 deletions(-)");
        assert_eq!((f, i, d), (5, 120, 34));
    }

    #[test]
    fn parse_diff_stat_singular_file() {
        let (f, i, d) = parse_diff_stat(" 1 file changed, 10 insertions(+)");
        assert_eq!((f, i, d), (1, 10, 0));
    }

    #[test]
    fn parse_diff_stat_deletions_only() {
        let (f, i, d) = parse_diff_stat(" 2 files changed, 3 deletions(-)");
        assert_eq!((f, i, d), (2, 0, 3));
    }

    #[test]
    fn parse_diff_stat_empty() {
        let (f, i, d) = parse_diff_stat("");
        assert_eq!((f, i, d), (0, 0, 0));
    }

    // ── status_command ──────────────────────────────────────────────────

    #[test]
    fn status_command_contains_all_section_markers() {
        let cfg = test_config();
        let cmd = status_command("target", &cfg);
        assert!(cmd.contains("__EPOCH__"));
        assert!(cmd.contains("__TMUX_START__"));
        assert!(cmd.contains("__TMUX_END__"));
        assert!(cmd.contains("__WORKTREES_START__"));
        assert!(cmd.contains("__WORKTREES_END__"));
        assert!(cmd.contains("__BRANCH_EXISTS_START__"));
        assert!(cmd.contains("__BRANCH_EXISTS_END__"));
        assert!(cmd.contains("__STATE_START__"));
        assert!(cmd.contains("__STATE_END__"));
        assert!(cmd.contains("__REVCOUNT_START__"));
        assert!(cmd.contains("__REVCOUNT_END__"));
        assert!(cmd.contains("__DIFFSTAT_START__"));
        assert!(cmd.contains("__DIFFSTAT_END__"));
    }

    #[test]
    fn status_command_references_default_branch_and_session_name() {
        let mut cfg = test_config();
        cfg.default_branch = "develop".into();
        let cmd = status_command("my-task", &cfg);
        assert!(cmd.contains("refs/heads/skulk-my-task"));
        assert!(cmd.contains("develop..skulk-my-task"));
        assert!(cmd.contains("develop...skulk-my-task"));
        assert!(cmd.contains("~/.skulk/state/skulk-my-task"));
    }

    // ── parse_status_output ─────────────────────────────────────────────

    #[test]
    fn parse_status_output_happy_path_idle() {
        let cfg = test_config();
        let raw = mock_status_output(
            1_700_000_200,
            "skulk-my-task\t1700000000\t1700000100\t0",
            &[("skulk-my-task", "/home/user/wt/skulk-my-task")],
            true,
            Some(1_700_000_150),
            Some(3),
            " 5 files changed, 120 insertions(+), 34 deletions(-)",
        );
        let view = parse_status_output(&raw, "my-task", &cfg).expect("should succeed");
        assert_eq!(view.display_name, "my-task");
        assert_eq!(view.state, AgentState::Idle);
        assert_eq!(view.uptime.as_deref(), Some("3m"));
        assert_eq!(view.branch, "skulk-my-task");
        assert_eq!(view.default_branch, "main");
        assert_eq!(view.commits_ahead, 3);
        assert_eq!(view.files_changed, 5);
        assert_eq!(view.insertions, 120);
        assert_eq!(view.deletions, 34);
        assert_eq!(
            view.worktree.as_deref(),
            Some("/home/user/wt/skulk-my-task")
        );
    }

    #[test]
    fn parse_status_output_stopped_has_full_git_info_and_no_uptime() {
        let cfg = test_config();
        // No tmux session, but worktree and branch still exist.
        let raw = mock_status_output(
            1_700_000_200,
            "no server running on /tmp/tmux-1000/default",
            &[("skulk-done", "/home/user/wt/skulk-done")],
            true,
            None,
            Some(2),
            " 1 file changed, 5 insertions(+)",
        );
        let view = parse_status_output(&raw, "done", &cfg).expect("should succeed");
        assert_eq!(view.state, AgentState::Stopped);
        assert!(view.uptime.is_none());
        assert_eq!(view.commits_ahead, 2);
        assert_eq!(view.files_changed, 1);
        assert_eq!(view.insertions, 5);
        assert_eq!(view.deletions, 0);
        assert_eq!(view.worktree.as_deref(), Some("/home/user/wt/skulk-done"));
    }

    #[test]
    fn parse_status_output_detached_session_with_commits() {
        let cfg = test_config();
        let raw = mock_status_output(
            1_700_000_200,
            "skulk-busy\t1700000000\t1700000180\t0",
            &[("skulk-busy", "/home/user/wt/skulk-busy")],
            true,
            None,
            Some(7),
            " 3 files changed, 40 insertions(+), 8 deletions(-)",
        );
        let view = parse_status_output(&raw, "busy", &cfg).expect("should succeed");
        assert_eq!(view.state, AgentState::Detached);
        assert_eq!(view.commits_ahead, 7);
        assert_eq!(view.uptime.as_deref(), Some("3m"));
    }

    #[test]
    fn parse_status_output_returns_not_found_when_nothing_exists() {
        let cfg = test_config();
        let raw = mock_status_output(
            1_700_000_200,
            "no server running on /tmp/tmux-1000/default",
            &[],
            false,
            None,
            None,
            "",
        );
        let err = parse_status_output(&raw, "ghost", &cfg).expect_err("should be NotFound");
        match err {
            SkulkError::NotFound(msg) => {
                assert!(msg.contains("ghost"));
                assert!(msg.contains("not found"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn parse_status_output_zero_commits_zero_files() {
        let cfg = test_config();
        let raw = mock_status_output(
            1_700_000_200,
            "skulk-fresh\t1700000000\t1700000100\t0",
            &[("skulk-fresh", "/home/user/wt/skulk-fresh")],
            true,
            None,
            Some(0),
            "",
        );
        let view = parse_status_output(&raw, "fresh", &cfg).expect("should succeed");
        assert_eq!(view.commits_ahead, 0);
        assert_eq!(view.files_changed, 0);
        assert_eq!(view.insertions, 0);
        assert_eq!(view.deletions, 0);
    }

    #[test]
    fn parse_status_output_branch_only_is_not_not_found() {
        // Branch exists but no session and no worktree — rare but shouldn't error.
        let cfg = test_config();
        let raw = mock_status_output(
            1_700_000_200,
            "no server running on /tmp/tmux-1000/default",
            &[],
            true,
            None,
            Some(1),
            " 1 file changed, 2 insertions(+)",
        );
        let view = parse_status_output(&raw, "lonely", &cfg).expect("should succeed");
        assert_eq!(view.state, AgentState::Stopped);
        assert!(view.worktree.is_none());
        assert_eq!(view.commits_ahead, 1);
    }

    // ── format_status ───────────────────────────────────────────────────

    fn sample_view() -> StatusView {
        StatusView {
            display_name: "my-task".into(),
            state: AgentState::Idle,
            uptime: Some("47m".into()),
            branch: "skulk-my-task".into(),
            default_branch: "main".into(),
            commits_ahead: 3,
            files_changed: 5,
            insertions: 120,
            deletions: 34,
            worktree: Some("~/wt/skulk-my-task".into()),
        }
    }

    #[test]
    fn format_status_has_all_fields() {
        let out = format_status_with_color(&sample_view(), false);
        assert!(out.contains("Agent:    my-task"));
        assert!(out.contains("Status:   idle"));
        assert!(out.contains("Uptime:   47m"));
        assert!(out.contains("Branch:   skulk-my-task"));
        assert!(out.contains("Commits:  3 ahead of main"));
        assert!(out.contains("Files:    5 changed (+120 -34)"));
        assert!(out.contains("Worktree: ~/wt/skulk-my-task"));
    }

    #[test]
    fn format_status_stopped_shows_dash_uptime() {
        let mut view = sample_view();
        view.state = AgentState::Stopped;
        view.uptime = None;
        let out = format_status_with_color(&view, false);
        assert!(out.contains("Status:   stopped"));
        assert!(out.contains("Uptime:   -"));
    }

    #[test]
    fn format_status_idle_is_bold_green_in_color() {
        let out = format_status_with_color(&sample_view(), true);
        assert!(out.contains("\x1b[32m"));
        assert!(out.contains("\x1b[1m"));
        assert!(out.contains("idle"));
    }

    #[test]
    fn format_status_stopped_is_yellow_in_color() {
        let mut view = sample_view();
        view.state = AgentState::Stopped;
        view.uptime = None;
        let out = format_status_with_color(&view, true);
        assert!(out.contains("\x1b[33m"));
        assert!(out.contains("stopped"));
    }

    #[test]
    fn format_status_no_color_when_disabled() {
        let out = format_status_with_color(&sample_view(), false);
        assert!(!out.contains("\x1b["));
    }

    #[test]
    fn format_status_zero_files_collapses_to_zero_changed() {
        let mut view = sample_view();
        view.files_changed = 0;
        view.insertions = 0;
        view.deletions = 0;
        let out = format_status_with_color(&view, false);
        assert!(out.contains("Files:    0 changed"));
    }

    #[test]
    fn format_status_dash_worktree_when_missing() {
        let mut view = sample_view();
        view.worktree = None;
        let out = format_status_with_color(&view, false);
        assert!(out.contains("Worktree: -"));
    }

    #[test]
    fn format_status_uses_custom_default_branch() {
        let mut view = sample_view();
        view.default_branch = "develop".into();
        let out = format_status_with_color(&view, false);
        assert!(out.contains("Commits:  3 ahead of develop"));
    }

    // ── cmd_status ──────────────────────────────────────────────────────

    #[test]
    fn cmd_status_runs_single_ssh_roundtrip_and_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_status_output(
            1_700_000_200,
            "skulk-test\t1700000000\t1700000100\t0",
            &[("skulk-test", "/wt/skulk-test")],
            true,
            Some(1_700_000_150),
            Some(2),
            " 2 files changed, 10 insertions(+), 3 deletions(-)",
        ))]);
        assert!(cmd_status(&ssh, "test", &cfg).is_ok());
        assert_eq!(
            ssh.calls().len(),
            1,
            "status must use a single SSH roundtrip"
        );
    }

    #[test]
    fn cmd_status_returns_not_found_when_agent_missing() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(mock_status_output(
            1_700_000_200,
            "no server running on /tmp/tmux-1000/default",
            &[],
            false,
            None,
            None,
            "",
        ))]);
        let result = cmd_status(&ssh, "ghost", &cfg);
        match result {
            Err(SkulkError::NotFound(msg)) => {
                assert!(msg.contains("ghost"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn cmd_status_rejects_invalid_name() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let result = cmd_status(&ssh, "../bad", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }
}
