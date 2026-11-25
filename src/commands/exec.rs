use std::io::Write as _;

use crate::agent::Agent;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::{SkulkError, classify_ssh_error};
use crate::ssh::{Ssh, StdoutMode};
use crate::util::{shell_escape, shell_quote_path};

/// Most bytes of each of stdout and stderr returned by `--json`.
pub(crate) const OUTPUT_CAP_BYTES: u64 = 1024 * 1024;

/// Exit code reported when `--timeout` kills the command, as GNU `timeout` does.
pub(crate) const TIMEOUT_EXIT_CODE: i32 = 124;

/// Most seconds between the `TERM` and the `KILL` sent to a timed-out
/// command's process group; the `KILL` is skipped once the group is gone.
const KILL_GRACE_SECS: u64 = 5;

/// Printed by the preflight script when the worktree exists; the worktree's
/// `.env` follows it, then [`SETTINGS_MARK`] and its
/// `.claude/settings.local.json`.
const READY: &str = "skulk-exec-ready";

/// Separates `.env` from the settings file in the preflight output.
const SETTINGS_MARK: &str = "\x1eskulk-exec-settings";

/// Starts the result line of the capture script.
const RESULT: &str = "skulk-exec-result";

/// Separates the stdout and stderr hex dumps of the capture script.
const STDERR_MARK: &str = "skulk-exec-stderr";

/// Starts each progress marker the script writes on its own stderr.
const MARK: u8 = 0x1e;

/// Progress marker written right before the command starts.
const STARTED: &str = "skulk-exec-started";

/// Progress marker written after the command ended: `<exit> <timed_out>`.
const EXITED: &str = "skulk-exec-exit";

/// Most characters of remote output quoted in an error message.
const QUOTE_CHARS: usize = 200;

/// What `skulk exec` runs and how.
pub(crate) struct ExecRequest<'a> {
    pub name: &'a str,
    pub argv: &'a [String],
    pub timeout_secs: Option<u64>,
}

/// The command's streams, decoded from the capture script's stdout.
#[derive(Debug, PartialEq, Eq)]
struct Captured {
    stdout: Vec<u8>,
    stdout_total: u64,
    stderr: Vec<u8>,
    stderr_total: u64,
}

/// Run `argv` in agent `name`'s worktree with the agent's environment.
///
/// Human output streams the command's stdout and stderr and ends with
/// [`SkulkError::CommandExit`] when it exits non-zero, so skulk exits with
/// the command's code. `--json` captures both streams (capped at
/// [`OUTPUT_CAP_BYTES`] each) and reports the exit code in the payload; a
/// failing command is not a skulk failure.
pub(crate) fn cmd_exec(ssh: &impl Ssh, req: &ExecRequest, cfg: &Config) -> Result<(), SkulkError> {
    let agent = Agent::resolve(req.name, cfg)?;
    if req.argv.is_empty() {
        return Err(SkulkError::Validation(
            "No command given. Usage: skulk exec <name> -- <cmd> [args…]".into(),
        ));
    }
    if req.timeout_secs == Some(0) {
        return Err(SkulkError::Validation(
            "--timeout must be at least 1 second.".into(),
        ));
    }

    let worktree = agent.worktree_path(cfg);
    let preflight = ssh
        .run(&preflight_script(&worktree))
        .map_err(|e| agent.classify(e))?;
    let Some(files) = preflight.strip_prefix(READY) else {
        return Err(SkulkError::NotFound(format!(
            "Agent '{}' has no worktree on {}. Check agents with `skulk list`.",
            req.name, cfg.host
        )));
    };
    let (dotenv, settings) = files.split_once(SETTINGS_MARK).unwrap_or((files, ""));
    let mut env = dotenv_vars(dotenv);
    env.extend(settings_env(settings));
    let input = env_payload(&env);

    let json = cfg.output_format == OutputFormat::Json;
    let script = exec_script(&agent, &worktree, req, json);
    let mut progress = Progress::default();
    if json {
        let (code, out) =
            ssh.run_with_input(&script, &input, StdoutMode::Capture, &mut |line| {
                progress.feed(line);
            })?;
        let exit = progress.finish(code, req.name, &cfg.host)?;
        let captured = parse_captured(&String::from_utf8_lossy(&out))?;
        emit_json(&json_exec_result(req.name, exit, &captured));
        return Ok(());
    }

    let mut stderr = std::io::stderr();
    let (code, _) = ssh.run_with_input(&script, &input, StdoutMode::Inherit, &mut |line| {
        let _ = stderr.write_all(progress.feed(line));
    })?;
    let (exit_code, timed_out) = progress.finish(code, req.name, &cfg.host)?;
    if timed_out {
        eprintln!(
            "skulk exec: timed out after {}s; command killed",
            req.timeout_secs.unwrap_or(0)
        );
    }
    if exit_code == 0 {
        Ok(())
    } else {
        Err(SkulkError::CommandExit(exit_code))
    }
}

/// What the script reported through the markers on its stderr.
#[derive(Debug, Default)]
struct Progress {
    started: bool,
    exit: Option<(i32, bool)>,
    /// Stderr written before the command started: the script's own errors.
    setup: Vec<u8>,
}

impl Progress {
    /// Record one stderr line and return the part of it that the command
    /// wrote. A marker can follow command output that lacked a final newline.
    fn feed<'a>(&mut self, line: &'a [u8]) -> &'a [u8] {
        let (output, marker) = match line.iter().position(|&b| b == MARK) {
            Some(pos) => (&line[..pos], Some(&line[pos + 1..])),
            None => (line, None),
        };
        let output = if self.started {
            output
        } else {
            self.setup.extend_from_slice(output);
            &[]
        };
        if let Some(marker) = marker {
            let marker = String::from_utf8_lossy(marker);
            let marker = marker.trim();
            if marker == STARTED {
                self.started = true;
            } else if let Some(fields) = marker.strip_prefix(EXITED) {
                let mut fields = fields.split_whitespace();
                if let (Some(Ok(code)), Some(timed_out)) =
                    (fields.next().map(str::parse), fields.next())
                {
                    self.exit = Some((code, timed_out == "1"));
                }
            }
        }
        output
    }

    /// The command's exit code and whether it timed out, or why the run
    /// produced none: the script never started the command (an SSH failure
    /// when the transport exited 255, a setup failure otherwise), or the
    /// connection was lost while it ran.
    fn finish(self, code: Option<i32>, name: &str, host: &str) -> Result<(i32, bool), SkulkError> {
        if let Some(exit) = self.exit {
            return Ok(exit);
        }
        let status = code.map_or_else(|| "a signal".to_string(), |c| format!("status {c}"));
        if self.started {
            return Err(SkulkError::SshFailed(format!(
                "Lost the connection to {host} while the command ran in agent '{name}' \
                 (ended by {status}); it was sent TERM."
            )));
        }
        let detail = quote(&String::from_utf8_lossy(&self.setup));
        if code == Some(255) {
            return Err(classify_ssh_error(&detail, host));
        }
        Err(SkulkError::Diagnostic {
            message: format!(
                "Could not start the command in agent '{name}' (ended by {status}): {detail}"
            ),
            suggestion: format!(
                "Check that bash is installed on {host} and that the worktree and $TMPDIR are writable."
            ),
        })
    }
}

/// At most [`QUOTE_CHARS`] characters of `text`, trimmed.
fn quote(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(QUOTE_CHARS) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

/// The `--json` payload.
fn json_exec_result(
    name: &str,
    (exit_code, timed_out): (i32, bool),
    c: &Captured,
) -> serde_json::Value {
    serde_json::json!({
        "agent": name,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "stdout": String::from_utf8_lossy(&c.stdout),
        "stderr": String::from_utf8_lossy(&c.stderr),
        "stdout_truncated": c.stdout_total > c.stdout.len() as u64,
        "stderr_truncated": c.stderr_total > c.stderr.len() as u64,
    })
}

/// Prints [`READY`], the worktree's `.env`, [`SETTINGS_MARK`] and its
/// `.claude/settings.local.json`, or nothing when the worktree is missing.
fn preflight_script(worktree: &str) -> String {
    let wt = shell_quote_path(worktree);
    format!(
        "cd {wt} 2>/dev/null || exit 0; printf '%s\\n' {READY}; \
         cat .env 2>/dev/null; printf '\\n\\036%s\\n' {mark}; \
         cat .claude/settings.local.json 2>/dev/null; exit 0",
        mark = &SETTINGS_MARK[1..],
    )
}

/// `KEY=VALUE` lines of a `.env` file, read as data: nothing is expanded or
/// executed. Blank lines, `#` comments, an `export ` prefix, keys that are not
/// shell identifiers and lines without `=` are skipped; one layer of matching
/// quotes is stripped from a value, and an unquoted value loses a trailing
/// ` # comment`.
fn dotenv_vars(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|raw| {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").map_or(line, str::trim_start);
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            is_shell_identifier(key).then(|| (key.to_string(), dotenv_value(value.trim())))
        })
        .collect()
}

fn dotenv_value(value: &str) -> String {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_string();
        }
    }
    let mut prev_blank = false;
    for (i, c) in value.char_indices() {
        if c == '#' && prev_blank {
            return value[..i].trim_end().to_string();
        }
        prev_blank = c == ' ' || c == '\t';
    }
    value.to_string()
}

/// The environment as the script reads it from stdin: `KEY=VALUE` records
/// ended by NUL, then an empty record. Values holding a NUL are dropped.
fn env_payload(env: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in env.iter().filter(|(_, v)| !v.contains('\0')) {
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
        out.push(0);
    }
    out.push(0);
    out
}

/// The `env` object of a Claude Code settings file, as `(name, value)` pairs
/// that can be exported from a shell. Strings, numbers and booleans are kept;
/// names that are not shell identifiers and other value types are skipped,
/// as is a file that does not parse.
fn settings_env(settings: &str) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(mut root)) = serde_json::from_str(settings) else {
        return Vec::new();
    };
    let Some(serde_json::Value::Object(env)) = root.remove("env") else {
        return Vec::new();
    };
    env.into_iter()
        .filter(|(k, _)| is_shell_identifier(k))
        .filter_map(|(k, v)| {
            let value = match v {
                serde_json::Value::String(s) => s,
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((k, value))
        })
        .collect()
}

fn is_shell_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Quote each argument as one shell word.
fn quote_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| format!("'{}'", shell_escape(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The bash script that runs the command, wrapped as `bash -c '…'`.
///
/// It exports `SKULK_*` and then the `KEY=VALUE` records read from stdin
/// (see [`env_payload`]) with `export "$kv"`, which never evaluates the value,
/// so secrets stay off every command line. It starts the command as a
/// background job under `set -m`, so the command and its children form their
/// own process group. bash, not `sh`: dash ignores `set -m` without a
/// terminal.
///
/// Every stop goes through `_reap`: `TERM` to the group, up to
/// [`KILL_GRACE_SECS`] for it to empty, then `KILL`. It runs after the
/// command's leader exits (so background children never outlive the run or
/// hold its streams open), from the timeout watchdog, from the stdin watcher
/// when stdin reaches EOF (skulk or its connection went away), and from the
/// `INT`/`TERM`/`HUP` trap.
///
/// With a timeout, the watchdog and the script race to create the `claim`
/// directory (`mkdir` is atomic), and the winner alone decides the outcome.
/// If the command ends first, the script claims it and cancels the watchdog.
/// If the watchdog claims it, it reaps the group and the script waits for
/// it. A watchdog whose `TERM` finds no group left records `gone`: the
/// command had already ended, so the run did not time out.
///
/// The temp dir goes away (an `EXIT` trap covers early exits) before the
/// script writes its report, and `SIGPIPE` is ignored from then on, so a
/// dead connection never leaves captured output behind in `$TMPDIR`.
///
/// On its own stderr the script writes a [`STARTED`] marker right before the
/// command starts and an [`EXITED`] marker (`exit timed_out`) after it ended
/// (see [`Progress`]), so a failure of the script or the connection is never
/// mistaken for the command's exit code. In capture mode the command's
/// streams go to temp files and the script prints a [`RESULT`] line
/// (`stdout_bytes stderr_bytes`) followed by hex dumps of the capped streams,
/// which survive lossy UTF-8 decoding. Otherwise the command writes to the
/// script's own streams.
fn exec_script(agent: &Agent, worktree: &str, req: &ExecRequest, capture: bool) -> String {
    let redirect = if capture {
        r#" >"$_st/o" 2>"$_st/e""#
    } else {
        ""
    };
    let watchdog = match req.timeout_secs {
        Some(secs) => format!(
            r#"( sleep {secs}; mkdir "$_st/claim" 2>/dev/null || exit 0
  _reap "$_sp" || : >"$_st/gone" ) </dev/null >/dev/null 2>&1 & _sw=$!
"#
        ),
        None => String::new(),
    };
    let report = if capture {
        format!(
            r#"_so=$(wc -c <"$_st/o"); _se=$(wc -c <"$_st/e")
exec 4<"$_st/o" 5<"$_st/e"; rm -rf "$_st"
printf '\036{EXITED} %s %s\n' "$_sc" "$_sx" >&2
printf '{RESULT} %s %s\n' $_so $_se
head -c {OUTPUT_CAP_BYTES} <&4 | od -An -v -tx1
echo {STDERR_MARK}
head -c {OUTPUT_CAP_BYTES} <&5 | od -An -v -tx1
"#
        )
    } else {
        format!(
            r#"rm -rf "$_st"
printf '\036{EXITED} %s %s\n' "$_sc" "$_sx" >&2
"#
        )
    };
    let script = format!(
        r#"exec 3<&0
cd {wt} 2>/dev/null || {{ echo 'skulk exec: cannot enter the worktree' >&2; exit 1; }}
export SKULK_AGENT_NAME='{name}' SKULK_SESSION='{session}' SKULK_BRANCH='{session}' SKULK_WORKTREE="$PWD"
while IFS= read -r -d '' _kv <&3 && [ -n "$_kv" ]; do export "$_kv"; done
_reap() {{
  kill -TERM -"$1" 2>/dev/null || return 1
  _i=0
  while [ "$_i" -lt {grace_tenths} ] && kill -0 -"$1" 2>/dev/null; do sleep 0.1; _i=$((_i + 1)); done
  kill -KILL -"$1" 2>/dev/null
  return 0
}}
_st=$(mktemp -d "${{TMPDIR:-/tmp}}/skulk-exec.XXXXXX") || {{ echo 'skulk exec: cannot create a temp directory' >&2; exit 1; }}
_sp= _sv= _sw=
trap 'rm -rf "$_st"' EXIT
trap 'for _g in $_sw $_sv; do kill -TERM -"$_g" 2>/dev/null; done; [ -n "$_sp" ] && _reap "$_sp"; exit 130' INT TERM HUP
set -m
printf '\036{STARTED}\n' >&2
{cmd} </dev/null 3<&-{redirect} & _sp=$!
( cat >/dev/null; _reap "$_sp" ) <&3 >/dev/null 2>&1 & _sv=$!
exec 3<&-
{watchdog}wait "$_sp" 2>/dev/null; _sc=$?; _sx=0
if [ -n "$_sw" ]; then
  if mkdir "$_st/claim" 2>/dev/null; then kill -TERM -"$_sw" 2>/dev/null
  else wait "$_sw" 2>/dev/null; [ -f "$_st/gone" ] || {{ _sc={TIMEOUT_EXIT_CODE}; _sx=1; }}
  fi
fi
kill -TERM -"$_sv" 2>/dev/null
_reap "$_sp"
set +m
trap '' PIPE
{report}exit 0
"#,
        wt = shell_quote_path(worktree),
        name = agent.name(),
        session = agent.session_name(),
        grace_tenths = KILL_GRACE_SECS * 10,
        cmd = quote_argv(req.argv),
    );
    format!("bash -c '{}'", shell_escape(&script))
}

/// Decode the capture script's output (see [`exec_script`]).
fn parse_captured(out: &str) -> Result<Captured, SkulkError> {
    let malformed =
        || SkulkError::SshFailed(format!("Unexpected output from skulk exec: {}", quote(out)));
    let mut lines = out.lines().skip_while(|l| !l.starts_with(RESULT));
    let header = lines.next().ok_or_else(malformed)?;
    let fields: Vec<&str> = header.split_whitespace().skip(1).collect();
    let [stdout_total, stderr_total] = fields[..] else {
        return Err(malformed());
    };
    let stdout_total = stdout_total.parse().map_err(|_| malformed())?;
    let stderr_total = stderr_total.parse().map_err(|_| malformed())?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut target = &mut stdout;
    for line in lines {
        if line.trim() == STDERR_MARK {
            target = &mut stderr;
            continue;
        }
        for byte in line.split_whitespace() {
            target.push(u8::from_str_radix(byte, 16).map_err(|_| malformed())?);
        }
    }
    Ok(Captured {
        stdout,
        stdout_total,
        stderr,
        stderr_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{InputRun, MockSsh, test_config};

    fn marker(text: &str) -> Vec<u8> {
        let mut line = vec![MARK];
        line.extend_from_slice(text.as_bytes());
        line.push(b'\n');
        line
    }

    fn markers(code: i32) -> Vec<Vec<u8>> {
        vec![marker(STARTED), marker(&format!("{EXITED} {code} 0"))]
    }

    fn input_run(code: Option<i32>, stderr: Vec<Vec<u8>>) -> InputRun {
        InputRun {
            code,
            stdout: Vec::new(),
            stderr,
        }
    }

    fn run_human(run: InputRun) -> Result<(), SkulkError> {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(READY.to_string())]).with_input_responses(vec![Ok(run)]);
        let args = argv(&["true"]);
        let req = ExecRequest {
            name: "a",
            argv: &args,
            timeout_secs: None,
        };
        cmd_exec(&ssh, &req, &cfg)
    }

    #[test]
    fn human_mode_succeeds_when_command_exits_zero() {
        assert!(run_human(input_run(Some(0), markers(0))).is_ok());
    }

    #[test]
    fn human_mode_returns_the_command_exit_code() {
        let result = run_human(input_run(Some(0), markers(7)));

        assert!(matches!(result, Err(SkulkError::CommandExit(7))));
    }

    #[test]
    fn human_mode_reports_255_from_the_command_as_its_exit_code() {
        let result = run_human(input_run(Some(0), markers(255)));

        assert!(matches!(result, Err(SkulkError::CommandExit(255))));
    }

    #[test]
    fn reports_ssh_failure_when_transport_exits_255_before_start() {
        let stderr = vec![b"ssh: connect to host h port 22: Connection refused\n".to_vec()];

        let err = run_human(input_run(Some(255), stderr)).unwrap_err();

        assert!(matches!(err, SkulkError::Diagnostic { .. }));
        assert!(err.to_string().contains("refused"), "{err}");
    }

    #[test]
    fn reports_setup_failure_with_its_message_when_script_fails_before_start() {
        let stderr = vec![b"skulk exec: cannot create a temp directory\n".to_vec()];

        let err = run_human(input_run(Some(1), stderr)).unwrap_err();

        assert!(matches!(err, SkulkError::Diagnostic { .. }));
        assert!(
            err.to_string().contains("Could not start the command"),
            "{err}"
        );
        assert!(
            err.to_string().contains("cannot create a temp directory"),
            "{err}"
        );
    }

    #[test]
    fn reports_lost_connection_when_started_but_no_exit_marker() {
        let err = run_human(input_run(Some(255), vec![marker(STARTED)])).unwrap_err();

        assert!(matches!(err, SkulkError::SshFailed(_)));
        assert!(err.to_string().contains("Lost the connection"), "{err}");
    }

    #[test]
    fn progress_passes_command_output_and_splits_a_marker_off_a_partial_line() {
        let mut progress = Progress::default();

        let before = progress.feed(b"setup noise\n").to_vec();
        progress.feed(&marker(STARTED));
        let output = progress.feed(b"out\n").to_vec();
        let mut tail = b"partial".to_vec();
        tail.extend(marker(&format!("{EXITED} 124 1")));
        let partial = progress.feed(&tail).to_vec();

        assert_eq!(before, b"");
        assert_eq!(output, b"out\n");
        assert_eq!(partial, b"partial");
        assert_eq!(progress.exit, Some((124, true)));
        assert_eq!(progress.setup, b"setup noise\n");
    }

    #[test]
    fn quote_shortens_long_output() {
        let long = "x".repeat(QUOTE_CHARS + 50);

        let quoted = quote(&long);

        assert_eq!(quoted.chars().count(), QUOTE_CHARS + 1);
        assert!(quoted.ends_with('…'));
    }

    #[test]
    fn malformed_capture_error_quotes_only_a_prefix() {
        let err = parse_captured(&"junk ".repeat(1000)).unwrap_err();

        assert!(err.to_string().len() < QUOTE_CHARS + 100, "{err}");
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    fn json_config() -> Config {
        let mut cfg = test_config();
        cfg.output_format = OutputFormat::Json;
        cfg
    }

    #[test]
    fn returns_not_found_when_worktree_is_missing() {
        let cfg = json_config();
        let ssh = MockSsh::new(vec![Ok(String::new())]);
        let args = argv(&["true"]);
        let req = ExecRequest {
            name: "gone",
            argv: &args,
            timeout_secs: None,
        };

        let result = cmd_exec(&ssh, &req, &cfg);

        assert!(matches!(result, Err(SkulkError::NotFound(_))));
        assert_eq!(ssh.calls().len(), 1);
    }

    #[test]
    fn rejects_empty_command() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let req = ExecRequest {
            name: "a",
            argv: &[],
            timeout_secs: None,
        };

        let result = cmd_exec(&ssh, &req, &cfg);

        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn rejects_zero_timeout() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![]);
        let args = argv(&["true"]);
        let req = ExecRequest {
            name: "a",
            argv: &args,
            timeout_secs: Some(0),
        };

        let result = cmd_exec(&ssh, &req, &cfg);

        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn json_mode_runs_capture_script_after_preflight() {
        let cfg = json_config();
        let ssh = MockSsh::new(vec![Ok(format!(
            "{READY}\nA=1\n{SETTINGS_MARK}\n{{\"env\":{{\"B\":\"2\"}}}}"
        ))])
        .with_input_responses(vec![Ok(InputRun {
            code: Some(0),
            stdout: format!("{RESULT} 2 0\n 68 69\n{STDERR_MARK}").into_bytes(),
            stderr: markers(3),
        })]);
        let args = argv(&["echo", "hi"]);
        let req = ExecRequest {
            name: "a",
            argv: &args,
            timeout_secs: None,
        };

        cmd_exec(&ssh, &req, &cfg).expect("exec succeeds despite non-zero exit");

        let calls = ssh.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].starts_with("bash -c "));
        assert!(calls[1].contains(RESULT));
        assert!(!calls[1].contains("A=1") && !calls[1].contains("B=2"));
        assert_eq!(ssh.inputs(), vec![b"A=1\0B=2\0\0".to_vec()]);
    }

    #[test]
    fn settings_env_keeps_scalars_with_valid_names() {
        let json = r#"{"hooks":{},"env":{"A":"x y","N":3,"B":true,"bad-name":"z","O":{}}}"#;

        let env = settings_env(json);

        assert_eq!(
            env,
            vec![
                ("A".to_string(), "x y".to_string()),
                ("B".to_string(), "true".to_string()),
                ("N".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn settings_env_is_empty_for_missing_or_invalid_file() {
        for file in ["", "not json", r#"{"hooks":{}}"#] {
            assert_eq!(settings_env(file), Vec::new(), "{file}");
        }
    }

    #[test]
    fn dotenv_vars_reads_lines_as_data() {
        let text = "# comment\n\nexport A=1\nB=\"two words\"\nC='$(touch x)'\n\
                    D=$(touch y)\nE=plain # note\nF=a#b\nbad-key=1\nnoequals\n";

        let vars = dotenv_vars(text);

        let expected = [
            ("A", "1"),
            ("B", "two words"),
            ("C", "$(touch x)"),
            ("D", "$(touch y)"),
            ("E", "plain"),
            ("F", "a#b"),
        ];
        assert_eq!(
            vars,
            expected
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn env_payload_ends_records_with_nul_and_drops_values_with_nul() {
        let env = vec![
            ("A".to_string(), "x\ny".to_string()),
            ("B".to_string(), "bad\0".to_string()),
        ];

        assert_eq!(env_payload(&env), b"A=x\ny\0\0".to_vec());
    }

    #[test]
    fn quote_argv_quotes_each_argument_as_one_word() {
        let quoted = quote_argv(&argv(&["echo", "a b", "it's", "$HOME"]));

        assert_eq!(quoted, r"'echo' 'a b' 'it'\''s' '$HOME'");
    }

    #[test]
    fn exec_script_adds_watchdog_only_with_timeout() {
        let cfg = test_config();
        let agent = Agent::resolve("a", &cfg).expect("valid name");
        let args = argv(&["true"]);
        let mut req = ExecRequest {
            name: "a",
            argv: &args,
            timeout_secs: None,
        };

        let without = exec_script(&agent, "/w", &req, true);
        req.timeout_secs = Some(7);
        let with = exec_script(&agent, "/w", &req, true);

        assert!(!without.contains("_sw=$!"));
        assert!(with.contains("_sw=$!"));
        assert!(with.contains("sleep 7"));
    }

    #[test]
    fn parse_captured_decodes_streams_and_flags() {
        let out = format!("noise\n{RESULT} 5 1\n 68 69\n 0a\n{STDERR_MARK}\n 21");

        let captured = parse_captured(&out).expect("parses");

        assert_eq!(
            captured,
            Captured {
                stdout: b"hi\n".to_vec(),
                stdout_total: 5,
                stderr: b"!".to_vec(),
                stderr_total: 1,
            }
        );
    }

    #[test]
    fn parse_captured_rejects_output_without_result_line() {
        assert!(parse_captured("whatever").is_err());
    }

    #[test]
    fn json_result_reports_truncation() {
        let c = Captured {
            stdout: b"ab".to_vec(),
            stdout_total: 10,
            stderr: Vec::new(),
            stderr_total: 0,
        };

        let v = json_exec_result("a", (0, false), &c);

        assert_eq!(v["stdout_truncated"], true);
        assert_eq!(v["stderr_truncated"], false);
        assert_eq!(v["stdout"], "ab");
    }
}
