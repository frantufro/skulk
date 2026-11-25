//! `skulk events`: one long-running stream of fleet changes.
//!
//! A single remote loop prints the fleet's state as a frame whenever it
//! changes (and `__SAME__` once a second otherwise). Skulk diffs consecutive
//! frames locally into `snapshot`, `idle_state`, `agent.created` and
//! `agent.gone` lines, and adds a `heartbeat` every 30 s. The stream ends with
//! an error, and so a non-zero exit, when the connection closes or the remote
//! falls silent.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};

use crate::commands::list::{agent_state_probe, parse_agent_sessions};
use crate::config::{Config, OutputFormat};
use crate::display::render_json;
use crate::error::SkulkError;
use crate::inventory::AgentState;
use crate::ssh::{LineStream, Ssh, StreamRecv};
use crate::timings::Clock;
use crate::util::extract_section;

const HEARTBEAT_MS: u64 = 30_000;
/// The remote prints at least once a second; this much silence means the
/// connection is gone even if SSH has not noticed yet.
const FRAME_TIMEOUT_MS: u64 = 20_000;
/// How long a session may be missing (with its worktree still there and no
/// archive record) before it is reported as crashed. `skulk destroy` kills
/// the session before removing the worktree, and this keeps that from
/// reading as a crash.
const CRASH_GRACE_MS: u64 = 3_000;

const FRAME_START: &str = "__FRAME_START__";
const FRAME_END: &str = "__FRAME_END__";
const SAME: &str = "__SAME__";

/// Lists archive sidecars with their mtime, one `<session> <epoch>` per line.
const ARCHIVE_PROBE: &str = "echo __ARCHIVE_START__; \
     for f in ~/.skulk/archive/*.txt; do [ -f \"$f\" ] && \
     printf '%s %s\\n' \"$(basename \"$f\" .txt)\" \"$(stat -c %Y \"$f\" 2>/dev/null || stat -f %m \"$f\")\"; \
     done 2>/dev/null; \
     echo __ARCHIVE_END__";

/// Waits up to a second for a marker or archive file to change. Falls back
/// to a plain one-second sleep without `inotifywait`, or when it fails
/// outright (exit 1), so the loop never spins.
const WAIT_FOR_CHANGE: &str = "if command -v inotifywait >/dev/null 2>&1; then \
     inotifywait -qq -t 1 -e close_write -e create -e delete -e moved_to \
     ~/.skulk/state ~/.skulk/archive >/dev/null 2>&1; [ $? -eq 1 ] && sleep 1; \
     else sleep 1; fi";

/// Build the remote watch loop.
pub(crate) fn events_command(cfg: &Config) -> String {
    let probe = agent_state_probe(cfg);
    format!(
        "mkdir -p ~/.skulk/state ~/.skulk/archive; prev=; \
         while :; do \
         cur=$({probe}; {ARCHIVE_PROBE}); \
         if [ \"$cur\" != \"$prev\" ]; then \
         printf '{FRAME_START}\\n%s\\n{FRAME_END}\\n' \"$cur\"; prev=$cur; \
         else echo {SAME}; fi; \
         {WAIT_FOR_CHANGE}; \
         done"
    )
}

/// What an agent's harness is doing, as the event stream reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleState {
    Working,
    Idle,
    Stopped,
}

impl IdleState {
    fn from_agent_state(state: AgentState) -> Self {
        match state {
            AgentState::Attached | AgentState::Detached => IdleState::Working,
            AgentState::Idle => IdleState::Idle,
            AgentState::Stopped => IdleState::Stopped,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            IdleState::Working => "working",
            IdleState::Idle => "idle",
            IdleState::Stopped => "stopped",
        }
    }
}

/// One agent as seen in a frame.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentView {
    pub state: IdleState,
    pub url: Option<String>,
    /// tmux `session_created` epoch; 0 when there is no session.
    pub created: i64,
}

/// The fleet as one remote frame describes it, keyed by agent name.
#[derive(Debug, Default)]
pub(crate) struct Frame {
    agents: BTreeMap<String, AgentView>,
    /// Agent name -> mtime of its archive sidecar.
    archived_at: HashMap<String, i64>,
}

/// Parse one frame (the agent state probe plus the archive section).
pub(crate) fn parse_frame(raw: &str, cfg: &Config) -> Frame {
    let short = |qualified: &str| {
        qualified
            .strip_prefix(&*cfg.session_prefix)
            .unwrap_or(qualified)
            .to_string()
    };
    let agents = parse_agent_sessions(raw, cfg)
        .into_iter()
        .map(|s| {
            let view = AgentView {
                state: IdleState::from_agent_state(s.state),
                url: s.url,
                created: s.created,
            };
            (short(&s.name), view)
        })
        .collect();
    let archived_at = extract_section(raw, "__ARCHIVE_START__\n", "\n__ARCHIVE_END__")
        .lines()
        .filter_map(|line| {
            let (session, mtime) = line.split_once(' ')?;
            if !session.starts_with(&*cfg.session_prefix) {
                return None;
            }
            Some((short(session), mtime.trim().parse().ok()?))
        })
        .collect();
    Frame {
        agents,
        archived_at,
    }
}

/// Why an agent left the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoneReason {
    /// Its worktree is gone.
    Destroyed,
    /// Its session was killed by `skulk archive`.
    Archived,
    /// Its session ended without an archive.
    Crashed,
}

impl GoneReason {
    fn as_str(self) -> &'static str {
        match self {
            GoneReason::Destroyed => "destroyed",
            GoneReason::Archived => "archived",
            GoneReason::Crashed => "crashed",
        }
    }
}

/// One line of the event stream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    Snapshot(Vec<(String, AgentView)>),
    IdleState {
        agent: String,
        from: IdleState,
        to: IdleState,
        url: Option<String>,
    },
    Created {
        agent: String,
        state: IdleState,
        url: Option<String>,
    },
    Gone {
        agent: String,
        reason: GoneReason,
    },
    Heartbeat,
}

#[derive(Debug)]
struct Tracked {
    /// The state last reported to the consumer.
    view: AgentView,
    /// Set while a vanished session waits out [`CRASH_GRACE_MS`].
    crash_pending_since: Option<u64>,
}

/// The fleet as last reported, and the rules that turn a new frame into
/// events.
#[derive(Debug, Default)]
pub(crate) struct Fleet {
    agents: BTreeMap<String, Tracked>,
}

impl Fleet {
    /// Start tracking from a first frame, returning the snapshot event.
    pub(crate) fn start(frame: &Frame) -> (Self, Event) {
        let agents = frame
            .agents
            .iter()
            .map(|(name, view)| {
                let tracked = Tracked {
                    view: view.clone(),
                    crash_pending_since: None,
                };
                (name.clone(), tracked)
            })
            .collect();
        let snapshot = frame
            .agents
            .iter()
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        (Self { agents }, Event::Snapshot(snapshot))
    }

    /// Compare `frame` with what was last reported and return the changes.
    pub(crate) fn apply(&mut self, frame: &Frame, now_ms: u64) -> Vec<Event> {
        let names: BTreeSet<String> = self
            .agents
            .keys()
            .chain(frame.agents.keys())
            .cloned()
            .collect();
        let mut events = Vec::new();
        for name in names {
            match (self.agents.get_mut(&name), frame.agents.get(&name)) {
                (None, Some(view)) => {
                    events.push(Event::Created {
                        agent: name.clone(),
                        state: view.state,
                        url: view.url.clone(),
                    });
                    let tracked = Tracked {
                        view: view.clone(),
                        crash_pending_since: None,
                    };
                    self.agents.insert(name, tracked);
                }
                (Some(_), None) => {
                    self.agents.remove(&name);
                    events.push(Event::Gone {
                        agent: name,
                        reason: GoneReason::Destroyed,
                    });
                }
                (Some(tracked), Some(view)) => {
                    let archived_at = frame.archived_at.get(&name).copied();
                    if let Some(event) = transition(&name, tracked, view, archived_at, now_ms) {
                        events.push(event);
                    }
                }
                (None, None) => {}
            }
        }
        events
    }
}

/// Move one agent that is in both the report and the frame to its new view.
fn transition(
    name: &str,
    tracked: &mut Tracked,
    next: &AgentView,
    archived_at: Option<i64>,
    now_ms: u64,
) -> Option<Event> {
    let from = tracked.view.state;
    match (from, next.state) {
        (IdleState::Stopped, IdleState::Stopped) => {
            tracked.view = next.clone();
            None
        }
        (_, IdleState::Stopped) => {
            let reason = if archived_at.is_some_and(|t| t >= tracked.view.created) {
                GoneReason::Archived
            } else {
                let since = *tracked.crash_pending_since.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) < CRASH_GRACE_MS {
                    return None;
                }
                GoneReason::Crashed
            };
            tracked.view = next.clone();
            tracked.crash_pending_since = None;
            Some(Event::Gone {
                agent: name.to_string(),
                reason,
            })
        }
        (_, to) => {
            tracked.view = next.clone();
            tracked.crash_pending_since = None;
            (from != to).then(|| Event::IdleState {
                agent: name.to_string(),
                from,
                to,
                url: next.url.clone(),
            })
        }
    }
}

/// Format epoch milliseconds as an RFC 3339 UTC timestamp with second
/// precision, e.g. `2026-11-24T10:31:02Z`.
pub(crate) fn rfc3339(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = secs % 86_400;
    // Civil-from-days, after Howard Hinnant's date algorithms.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The JSON line for `event` (without `"v"`, which `render_json` adds).
pub(crate) fn event_json(event: &Event, ts: &str) -> Value {
    match event {
        Event::Snapshot(agents) => {
            let agents: Vec<Value> = agents
                .iter()
                .map(|(name, view)| {
                    json!({ "name": name, "state": view.state.as_str(), "url": view.url })
                })
                .collect();
            json!({ "ts": ts, "type": "snapshot", "agents": agents })
        }
        Event::IdleState {
            agent,
            from,
            to,
            url,
        } => json!({
            "ts": ts,
            "type": "idle_state",
            "agent": agent,
            "from": from.as_str(),
            "to": to.as_str(),
            "url": url,
        }),
        Event::Created { agent, state, url } => json!({
            "ts": ts,
            "type": "agent.created",
            "agent": agent,
            "state": state.as_str(),
            "url": url,
        }),
        Event::Gone { agent, reason } => json!({
            "ts": ts,
            "type": "agent.gone",
            "agent": agent,
            "reason": reason.as_str(),
        }),
        Event::Heartbeat => json!({ "ts": ts, "type": "heartbeat" }),
    }
}

/// The human-readable line for `event`, or `None` for heartbeats.
pub(crate) fn event_human(event: &Event, ts: &str) -> Option<String> {
    let with_url = |url: &Option<String>| url.as_ref().map_or(String::new(), |u| format!(" {u}"));
    let line = match event {
        Event::Snapshot(agents) if agents.is_empty() => format!("{ts} no agents"),
        Event::Snapshot(agents) => agents
            .iter()
            .map(|(name, view)| {
                format!(
                    "{ts} {name}: {}{}",
                    view.state.as_str(),
                    with_url(&view.url)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Event::IdleState {
            agent,
            from,
            to,
            url,
        } => format!(
            "{ts} {agent}: {} -> {}{}",
            from.as_str(),
            to.as_str(),
            with_url(url)
        ),
        Event::Created { agent, state, url } => {
            format!(
                "{ts} {agent}: created ({}){}",
                state.as_str(),
                with_url(url)
            )
        }
        Event::Gone { agent, reason } => format!("{ts} {agent}: gone ({})", reason.as_str()),
        Event::Heartbeat => return None,
    };
    Some(line)
}

fn emit(
    event: &Event,
    now_ms: u64,
    format: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), SkulkError> {
    let ts = rfc3339(now_ms);
    let line = match format {
        OutputFormat::Json => Some(
            render_json(&event_json(event, &ts))
                .map_err(|e| SkulkError::Output(format!("json serialization failed: {e}")))?,
        ),
        OutputFormat::Human => event_human(event, &ts),
    };
    if let Some(line) = line {
        writeln!(out, "{line}")
            .and_then(|()| out.flush())
            .map_err(|e| SkulkError::Output(e.to_string()))?;
    }
    Ok(())
}

/// Collects stream lines into frames.
#[derive(Default)]
struct FrameReader {
    buf: Option<String>,
}

enum Tick {
    Frame(String),
    Same,
}

impl FrameReader {
    fn push(&mut self, line: &str) -> Option<Tick> {
        match line {
            FRAME_START => {
                self.buf = Some(String::new());
                None
            }
            FRAME_END => self.buf.take().map(Tick::Frame),
            SAME => Some(Tick::Same),
            _ => {
                if let Some(buf) = &mut self.buf {
                    buf.push_str(line);
                    buf.push('\n');
                }
                None
            }
        }
    }
}

/// `skulk events`: stream fleet changes until the connection drops.
pub(crate) fn cmd_events(
    ssh: &impl Ssh,
    cfg: &Config,
    clock: &dyn Clock,
) -> Result<(), SkulkError> {
    let mut stream = ssh.stream(&events_command(cfg))?;
    run_events(stream.as_mut(), cfg, clock, &mut std::io::stdout().lock())
}

/// The event loop behind [`cmd_events`]. Returns only with an error: the
/// stream closed, went silent for [`FRAME_TIMEOUT_MS`], or `out` failed.
fn run_events(
    stream: &mut dyn LineStream,
    cfg: &Config,
    clock: &dyn Clock,
    out: &mut dyn Write,
) -> Result<(), SkulkError> {
    let mut reader = FrameReader::default();
    let mut latest: Option<Frame> = None;
    let mut fleet: Option<Fleet> = None;
    let mut last_line_ms = clock.now_ms();
    let mut next_heartbeat_ms: Option<u64> = None;
    loop {
        let now = clock.now_ms();
        let frame_deadline = last_line_ms + FRAME_TIMEOUT_MS;
        if now >= frame_deadline {
            return Err(SkulkError::SshFailed(format!(
                "No update from {} for {}s; treating the connection as lost.",
                cfg.host,
                FRAME_TIMEOUT_MS / 1000
            )));
        }
        if let Some(due) = next_heartbeat_ms
            && now >= due
        {
            emit(&Event::Heartbeat, now, cfg.output_format, out)?;
            next_heartbeat_ms = Some(now + HEARTBEAT_MS);
        }
        let wake = next_heartbeat_ms.map_or(frame_deadline, |hb| hb.min(frame_deadline));
        let line = match stream.recv(Duration::from_millis(wake.saturating_sub(now))) {
            StreamRecv::Line(line) => line,
            StreamRecv::Timeout => continue,
            StreamRecv::Closed(e) => return Err(e),
        };
        let now = clock.now_ms();
        last_line_ms = now;
        match reader.push(&line) {
            Some(Tick::Frame(raw)) => latest = Some(parse_frame(&raw, cfg)),
            Some(Tick::Same) => {}
            None => continue,
        }
        let Some(frame) = &latest else { continue };
        let events = if let Some(fleet) = &mut fleet {
            fleet.apply(frame, now)
        } else {
            let (started, snapshot) = Fleet::start(frame);
            fleet = Some(started);
            next_heartbeat_ms = Some(now + HEARTBEAT_MS);
            vec![snapshot]
        };
        for event in &events {
            emit(event, now, cfg.output_format, out)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::fmt::Write as _;
    use std::rc::Rc;

    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, mock_list_output_with_state, mock_urls_section, test_config,
        test_config_json,
    };

    // ── Fixtures ────────────────────────────────────────────────────────

    /// One agent in a test frame.
    struct A {
        name: &'static str,
        session: bool,
        created: i64,
        marker: Option<&'static str>,
        worktree: bool,
        url: Option<&'static str>,
    }

    fn working(name: &'static str) -> A {
        A {
            name,
            session: true,
            created: 1_000,
            marker: Some("busy"),
            worktree: true,
            url: None,
        }
    }

    fn idle(name: &'static str) -> A {
        A {
            marker: Some("idle"),
            ..working(name)
        }
    }

    fn stopped(name: &'static str) -> A {
        A {
            session: false,
            created: 0,
            marker: None,
            ..working(name)
        }
    }

    /// Raw frame text as the remote probe prints it. `archives` are
    /// `(agent name, sidecar mtime)` pairs.
    fn frame_text(agents: &[A], archives: &[(&str, i64)]) -> String {
        let mut tmux = String::new();
        let mut worktrees = Vec::new();
        let mut state = Vec::new();
        let mut urls = Vec::new();
        let sessions: Vec<String> = agents.iter().map(|a| format!("skulk-{}", a.name)).collect();
        let paths: Vec<String> = agents
            .iter()
            .map(|a| format!("/wt/skulk-{}", a.name))
            .collect();
        for ((a, session), path) in agents.iter().zip(&sessions).zip(&paths) {
            if a.session {
                writeln!(tmux, "{session}\t{}\t0", a.created).unwrap();
            }
            if a.worktree {
                worktrees.push((session.as_str(), path.as_str()));
            }
            if let Some(m) = a.marker {
                state.push((session.as_str(), m));
            }
            if let Some(u) = a.url {
                urls.push((path.as_str(), u));
            }
        }
        let mut out = mock_list_output_with_state(0, &tmux, &worktrees, &state);
        out.push_str(&mock_urls_section(&urls));
        out.push_str("__ARCHIVE_START__\n");
        for (name, mtime) in archives {
            writeln!(out, "skulk-{name} {mtime}").unwrap();
        }
        out.push_str("__ARCHIVE_END__\n");
        out
    }

    fn frame(agents: &[A], archives: &[(&str, i64)]) -> Frame {
        parse_frame(&frame_text(agents, archives), &test_config())
    }

    /// The stream lines for one frame.
    fn frame_lines(agents: &[A], archives: &[(&str, i64)]) -> Vec<String> {
        let text = frame_text(agents, archives);
        let mut lines = vec![FRAME_START.to_string()];
        lines.extend(text.trim_end_matches('\n').lines().map(String::from));
        lines.push(FRAME_END.to_string());
        lines
    }

    struct FakeClock(Rc<Cell<u64>>);

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }

    /// Delivers each line at its scheduled time on the shared fake clock.
    /// Once the lines run out it closes at `closes_at`, or stays silent
    /// forever when that is `None`.
    struct TestStream {
        clock: Rc<Cell<u64>>,
        lines: VecDeque<(u64, String)>,
        closes_at: Option<u64>,
    }

    impl LineStream for TestStream {
        fn recv(&mut self, timeout: Duration) -> StreamRecv {
            let now = self.clock.get();
            let limit = now + u64::try_from(timeout.as_millis()).unwrap();
            if let Some((at, _)) = self.lines.front()
                && *at <= limit
            {
                let (at, line) = self.lines.pop_front().unwrap();
                self.clock.set(now.max(at));
                return StreamRecv::Line(line);
            }
            if self.lines.is_empty()
                && let Some(close) = self.closes_at
                && close <= limit
            {
                self.clock.set(now.max(close));
                return StreamRecv::Closed(SkulkError::SshFailed("connection closed".into()));
            }
            self.clock.set(limit);
            StreamRecv::Timeout
        }
    }

    /// Run the event loop over `lines` (each `(at_ms, line)`) and return its
    /// error and the printed lines.
    fn run(
        lines: Vec<(u64, String)>,
        closes_at: Option<u64>,
        cfg: &Config,
    ) -> (SkulkError, Vec<String>) {
        let clock = Rc::new(Cell::new(0));
        let mut stream = TestStream {
            clock: Rc::clone(&clock),
            lines: lines.into(),
            closes_at,
        };
        let mut out = Vec::new();
        let err = run_events(&mut stream, cfg, &FakeClock(clock), &mut out).unwrap_err();
        let text = String::from_utf8(out).unwrap();
        (err, text.lines().map(String::from).collect())
    }

    fn at(ms: u64, lines: Vec<String>) -> Vec<(u64, String)> {
        lines.into_iter().map(|l| (ms, l)).collect()
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    // ── events_command ──────────────────────────────────────────────────

    #[test]
    fn events_command_loops_the_state_probe_and_archive_listing() {
        let cfg = test_config();

        let cmd = events_command(&cfg);

        assert!(cmd.contains("while :; do"));
        assert!(cmd.contains(&agent_state_probe(&cfg)));
        assert!(cmd.contains("~/.skulk/archive/*.txt"));
        assert!(cmd.contains(FRAME_START) && cmd.contains(FRAME_END) && cmd.contains(SAME));
    }

    #[test]
    fn events_command_waits_at_most_one_second_between_probes() {
        let cmd = events_command(&test_config());

        assert!(cmd.contains("inotifywait -qq -t 1"));
        assert!(cmd.contains("~/.skulk/state ~/.skulk/archive"));
        assert!(cmd.contains("else sleep 1; fi"));
    }

    // ── parse_frame ─────────────────────────────────────────────────────

    #[test]
    fn parse_frame_maps_sessions_markers_and_worktrees_to_idle_states() {
        let f = frame(
            &[
                working("a"),
                idle("b"),
                stopped("c"),
                A {
                    marker: None,
                    ..working("d")
                },
            ],
            &[],
        );

        let states: Vec<(&str, IdleState)> = f
            .agents
            .iter()
            .map(|(n, v)| (n.as_str(), v.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("a", IdleState::Working),
                ("b", IdleState::Idle),
                ("c", IdleState::Stopped),
                ("d", IdleState::Working),
            ]
        );
    }

    #[test]
    fn parse_frame_reads_url_and_archive_mtimes() {
        let f = frame(
            &[A {
                url: Some("http://host:3001"),
                ..working("a")
            }],
            &[("a", 1_500)],
        );

        assert_eq!(f.agents["a"].url.as_deref(), Some("http://host:3001"));
        assert_eq!(f.archived_at.get("a"), Some(&1_500));
    }

    #[test]
    fn parse_frame_ignores_archive_records_of_other_prefixes() {
        let raw = format!(
            "{}__ARCHIVE_START__\nother-a 5\n__ARCHIVE_END__\n",
            frame_text(&[], &[]).replace("__ARCHIVE_START__\n__ARCHIVE_END__\n", "")
        );

        let f = parse_frame(&raw, &test_config());

        assert!(f.archived_at.is_empty());
    }

    // ── Fleet ───────────────────────────────────────────────────────────

    #[test]
    fn start_reports_a_snapshot_of_every_agent() {
        let (_, event) = Fleet::start(&frame(&[working("a"), stopped("b")], &[]));

        let Event::Snapshot(agents) = event else {
            panic!("expected snapshot")
        };
        let names: Vec<&str> = agents.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn apply_reports_idle_state_change_with_url() {
        let (mut fleet, _) = Fleet::start(&frame(&[working("a")], &[]));
        let next = frame(
            &[A {
                url: Some("http://x"),
                ..idle("a")
            }],
            &[],
        );

        let events = fleet.apply(&next, 0);

        assert_eq!(
            events,
            vec![Event::IdleState {
                agent: "a".into(),
                from: IdleState::Working,
                to: IdleState::Idle,
                url: Some("http://x".into()),
            }]
        );
    }

    #[test]
    fn apply_reports_nothing_when_idle_state_is_unchanged() {
        let (mut fleet, _) = Fleet::start(&frame(&[working("a"), stopped("b")], &[]));

        let events = fleet.apply(&frame(&[working("a"), stopped("b")], &[]), 0);

        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn apply_reports_created_for_a_new_agent() {
        let (mut fleet, _) = Fleet::start(&frame(&[], &[]));

        let events = fleet.apply(&frame(&[working("a")], &[]), 0);

        assert_eq!(
            events,
            vec![Event::Created {
                agent: "a".into(),
                state: IdleState::Working,
                url: None,
            }]
        );
    }

    #[test]
    fn apply_reports_destroyed_when_agent_disappears() {
        let (mut fleet, _) = Fleet::start(&frame(&[working("a")], &[]));

        let events = fleet.apply(&frame(&[], &[]), 0);

        assert_eq!(
            events,
            vec![Event::Gone {
                agent: "a".into(),
                reason: GoneReason::Destroyed,
            }]
        );
    }

    #[test]
    fn apply_reports_archived_when_sidecar_is_newer_than_session() {
        let (mut fleet, _) = Fleet::start(&frame(&[working("a")], &[]));

        let events = fleet.apply(&frame(&[stopped("a")], &[("a", 2_000)]), 0);

        assert_eq!(
            events,
            vec![Event::Gone {
                agent: "a".into(),
                reason: GoneReason::Archived,
            }]
        );
    }

    #[test]
    fn apply_waits_out_the_grace_period_before_reporting_a_crash() {
        let (mut fleet, _) = Fleet::start(&frame(&[working("a")], &[]));
        let gone = frame(&[stopped("a")], &[]);

        let early = fleet.apply(&gone, 10_000);
        let still_early = fleet.apply(&gone, 10_000 + CRASH_GRACE_MS - 1);
        let late = fleet.apply(&gone, 10_000 + CRASH_GRACE_MS);

        assert!(early.is_empty() && still_early.is_empty());
        assert_eq!(
            late,
            vec![Event::Gone {
                agent: "a".into(),
                reason: GoneReason::Crashed,
            }]
        );
    }

    #[test]
    fn apply_reports_only_destroyed_when_worktree_goes_within_grace() {
        let (mut fleet, _) = Fleet::start(&frame(&[working("a")], &[]));
        fleet.apply(&frame(&[stopped("a")], &[]), 0);

        let events = fleet.apply(&frame(&[], &[]), 1_000);

        assert_eq!(
            events,
            vec![Event::Gone {
                agent: "a".into(),
                reason: GoneReason::Destroyed,
            }]
        );
    }

    #[test]
    fn apply_treats_an_archive_older_than_the_session_as_a_crash() {
        let (mut fleet, _) = Fleet::start(&frame(&[working("a")], &[("a", 500)]));
        let gone = frame(&[stopped("a")], &[("a", 500)]);

        fleet.apply(&gone, 0);
        let events = fleet.apply(&gone, CRASH_GRACE_MS);

        assert_eq!(
            events,
            vec![Event::Gone {
                agent: "a".into(),
                reason: GoneReason::Crashed,
            }]
        );
    }

    #[test]
    fn apply_reports_a_restart_as_stopped_to_working() {
        let (mut fleet, _) = Fleet::start(&frame(&[stopped("a")], &[]));

        let events = fleet.apply(&frame(&[working("a")], &[]), 0);

        assert_eq!(
            events,
            vec![Event::IdleState {
                agent: "a".into(),
                from: IdleState::Stopped,
                to: IdleState::Working,
                url: None,
            }]
        );
    }

    // ── Rendering ───────────────────────────────────────────────────────

    #[test]
    fn rfc3339_formats_utc_seconds() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_516_262_999), "2026-11-24T10:31:02Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn event_json_matches_the_documented_line_format() {
        let ts = "2026-11-24T10:33:40Z";
        let line = |e: &Event| parse(&render_json(&event_json(e, ts)).unwrap());

        let snapshot = line(&Event::Snapshot(vec![(
            "fix-bug".into(),
            AgentView {
                state: IdleState::Working,
                url: Some("http://h:1".into()),
                created: 1,
            },
        )]));
        let change = line(&Event::IdleState {
            agent: "fix-bug".into(),
            from: IdleState::Working,
            to: IdleState::Idle,
            url: None,
        });
        let created = line(&Event::Created {
            agent: "auth".into(),
            state: IdleState::Working,
            url: None,
        });
        let gone = line(&Event::Gone {
            agent: "explore".into(),
            reason: GoneReason::Archived,
        });
        let heartbeat = line(&Event::Heartbeat);

        assert_eq!(
            snapshot,
            json!({"v":1,"ts":ts,"type":"snapshot","agents":[{"name":"fix-bug","state":"working","url":"http://h:1"}]})
        );
        assert_eq!(
            change,
            json!({"v":1,"ts":ts,"type":"idle_state","agent":"fix-bug","from":"working","to":"idle","url":null})
        );
        assert_eq!(
            created,
            json!({"v":1,"ts":ts,"type":"agent.created","agent":"auth","state":"working","url":null})
        );
        assert_eq!(
            gone,
            json!({"v":1,"ts":ts,"type":"agent.gone","agent":"explore","reason":"archived"})
        );
        assert_eq!(heartbeat, json!({"v":1,"ts":ts,"type":"heartbeat"}));
    }

    #[test]
    fn event_human_prints_readable_lines_and_skips_heartbeats() {
        let ts = "T";
        let change = Event::IdleState {
            agent: "a".into(),
            from: IdleState::Working,
            to: IdleState::Idle,
            url: Some("http://x".into()),
        };
        let gone = Event::Gone {
            agent: "a".into(),
            reason: GoneReason::Crashed,
        };

        assert_eq!(
            event_human(&change, ts).as_deref(),
            Some("T a: working -> idle http://x")
        );
        assert_eq!(
            event_human(&gone, ts).as_deref(),
            Some("T a: gone (crashed)")
        );
        assert_eq!(
            event_human(&Event::Snapshot(vec![]), ts).as_deref(),
            Some("T no agents")
        );
        assert_eq!(event_human(&Event::Heartbeat, ts), None);
    }

    // ── run_events ──────────────────────────────────────────────────────

    #[test]
    fn run_events_prints_snapshot_first() {
        let cfg = test_config_json();

        let (_, lines) = run(at(500, frame_lines(&[working("a")], &[])), Some(600), &cfg);

        assert_eq!(lines.len(), 1);
        let snapshot = parse(&lines[0]);
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(snapshot["agents"][0]["name"], "a");
    }

    #[test]
    fn run_events_prints_a_state_change_as_soon_as_its_frame_arrives() {
        let cfg = test_config_json();
        let mut lines = at(0, frame_lines(&[working("a")], &[]));
        lines.extend(at(1_000, vec![SAME.to_string()]));
        lines.extend(at(1_700, frame_lines(&[idle("a")], &[])));
        let clock = Rc::new(Cell::new(0));
        let mut stream = TestStream {
            clock: Rc::clone(&clock),
            lines: lines.into(),
            closes_at: Some(1_700),
        };
        let mut out = Vec::new();

        run_events(&mut stream, &cfg, &FakeClock(clock), &mut out).unwrap_err();

        let printed: Vec<Value> = String::from_utf8(out).unwrap().lines().map(parse).collect();
        assert_eq!(printed.len(), 2);
        assert_eq!(printed[1]["type"], "idle_state");
        assert_eq!(printed[1]["from"], "working");
        assert_eq!(printed[1]["to"], "idle");
        assert_eq!(printed[1]["ts"], rfc3339(1_700));
    }

    #[test]
    fn run_events_resolves_a_pending_crash_on_unchanged_frames() {
        let cfg = test_config_json();
        let mut lines = at(0, frame_lines(&[working("a")], &[]));
        lines.extend(at(1_000, frame_lines(&[stopped("a")], &[])));
        lines.extend((2..=5).map(|s| (s * 1_000, SAME.to_string())));

        let (_, printed) = run(lines, Some(5_500), &cfg);

        assert_eq!(printed.len(), 2, "{printed:?}");
        let gone = parse(&printed[1]);
        assert_eq!(gone["type"], "agent.gone");
        assert_eq!(gone["reason"], "crashed");
        assert_eq!(gone["ts"], rfc3339(1_000 + CRASH_GRACE_MS));
    }

    #[test]
    fn run_events_sends_a_heartbeat_every_thirty_seconds() {
        let cfg = test_config_json();
        let mut lines = at(0, frame_lines(&[], &[]));
        lines.extend((1..=65).map(|s| (s * 1_000, SAME.to_string())));

        let (_, printed) = run(lines, Some(65_500), &cfg);

        let types: Vec<String> = printed
            .iter()
            .map(|l| parse(l)["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(types, vec!["snapshot", "heartbeat", "heartbeat"]);
        assert_eq!(parse(&printed[1])["ts"], rfc3339(30_000));
        assert_eq!(parse(&printed[2])["ts"], rfc3339(60_000));
    }

    #[test]
    fn run_events_returns_error_when_the_connection_closes() {
        let cfg = test_config_json();

        let (err, _) = run(at(0, frame_lines(&[], &[])), Some(3_000), &cfg);

        assert_err!(Err::<(), _>(err), SkulkError::SshFailed(msg) => {
            assert_eq!(msg, "connection closed");
        });
    }

    #[test]
    fn run_events_returns_error_when_the_remote_falls_silent() {
        let cfg = test_config_json();

        let (err, printed) = run(at(0, frame_lines(&[], &[])), None, &cfg);

        assert_eq!(printed.len(), 1);
        assert_err!(Err::<(), _>(err), SkulkError::SshFailed(msg) => {
            assert!(msg.contains("No update from testhost for 20s"), "{msg}");
        });
    }

    #[test]
    fn run_events_returns_error_when_no_frame_ever_arrives() {
        let (err, printed) = run(vec![], None, &test_config_json());

        assert_eq!(printed, Vec::<String>::new());
        assert!(matches!(err, SkulkError::SshFailed(_)));
    }

    #[test]
    fn run_events_prints_human_lines_without_json() {
        let mut lines = at(0, frame_lines(&[working("a")], &[]));
        lines.extend(at(1_000, frame_lines(&[idle("a")], &[])));

        let (_, printed) = run(lines, Some(1_500), &test_config());

        assert_eq!(
            printed,
            vec![
                format!("{} a: working", rfc3339(0)),
                format!("{} a: working -> idle", rfc3339(1_000)),
            ]
        );
    }

    // ── cmd_events ──────────────────────────────────────────────────────

    #[test]
    fn cmd_events_opens_one_stream_with_the_watch_loop() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![]).with_stream_responses(vec![Ok(vec![])]);

        let result = cmd_events(&ssh, &cfg, &FakeClock(Rc::new(Cell::new(0))));

        assert!(result.is_err());
        assert_eq!(ssh.calls(), vec![events_command(&cfg)]);
    }

    #[test]
    fn cmd_events_returns_error_when_the_stream_cannot_start() {
        let cfg = test_config_json();
        let ssh = MockSsh::new(vec![])
            .with_stream_responses(vec![Err(SkulkError::SshExec("no ssh".into()))]);

        let result = cmd_events(&ssh, &cfg, &FakeClock(Rc::new(Cell::new(0))));

        assert_err!(result, SkulkError::SshExec(_) => {});
    }
}
