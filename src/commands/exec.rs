use std::fmt::Write as _;

use crate::agent::Agent;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::{shell_escape, shell_quote_path};

/// Most bytes of each of stdout and stderr returned by `--json`.
pub(crate) const OUTPUT_CAP_BYTES: u64 = 1024 * 1024;

/// Exit code reported when `--timeout` kills the command, as GNU `timeout` does.
pub(crate) const TIMEOUT_EXIT_CODE: i32 = 124;

/// Seconds between the `TERM` and the `KILL` sent to a timed-out command.
const KILL_GRACE_SECS: u64 = 5;

/// Printed by the preflight script when the worktree exists; the agent's
/// `.claude/settings.local.json` (if any) follows it.
const READY: &str = "skulk-exec-ready";

/// Starts the result line of the capture script.
const RESULT: &str = "skulk-exec-result";

/// Separates the stdout and stderr hex dumps of the capture script.
const STDERR_MARK: &str = "skulk-exec-stderr";

/// What `skulk exec` runs and how.
pub(crate) struct ExecRequest<'a> {
    pub name: &'a str,
    pub argv: &'a [String],
    pub timeout_secs: Option<u64>,
}

/// The outcome of a captured run, decoded from the capture script's output.
#[derive(Debug, PartialEq, Eq)]
struct Captured {
    exit_code: i32,
    timed_out: bool,
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
    let Some(settings) = preflight.strip_prefix(READY) else {
        return Err(SkulkError::NotFound(format!(
            "Agent '{}' has no worktree on {}. Check agents with `skulk list`.",
            req.name, cfg.host
        )));
    };
    let env = settings_env(settings);

    let json = cfg.output_format == OutputFormat::Json;
    let script = exec_script(&agent, &worktree, &env, req, json);
    if json {
        let out = ssh.run(&script).map_err(|e| agent.classify(e))?;
        let captured = parse_captured(&out)?;
        emit_json(&json_exec_result(req.name, &captured));
        return Ok(());
    }

    let status = ssh.interactive(&script)?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(SkulkError::CommandExit(code)),
        None => Err(SkulkError::SshFailed(format!(
            "Command in agent '{}' ended without an exit code ({status}).",
            req.name
        ))),
    }
}

/// The `--json` payload.
fn json_exec_result(name: &str, c: &Captured) -> serde_json::Value {
    serde_json::json!({
        "agent": name,
        "exit_code": c.exit_code,
        "timed_out": c.timed_out,
        "stdout": String::from_utf8_lossy(&c.stdout),
        "stderr": String::from_utf8_lossy(&c.stderr),
        "stdout_truncated": c.stdout_total > c.stdout.len() as u64,
        "stderr_truncated": c.stderr_total > c.stderr.len() as u64,
    })
}

/// Prints [`READY`] and the worktree's `.claude/settings.local.json`, or
/// nothing when the worktree is missing.
fn preflight_script(worktree: &str) -> String {
    let wt = shell_quote_path(worktree);
    format!(
        "cd {wt} 2>/dev/null || exit 0; printf '%s' {READY}; \
         cat .claude/settings.local.json 2>/dev/null; exit 0"
    )
}

/// The `env` object of a Claude Code settings file, as `(name, value)` pairs
/// that can be exported from a shell. Strings, numbers and booleans are kept;
/// names that are not shell identifiers and other value types are skipped,
/// as is a file that does not parse.
fn settings_env(settings: &str) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str(settings) else {
        return Vec::new();
    };
    let Some(serde_json::Value::Object(env)) = root.get("env") else {
        return Vec::new();
    };
    env.iter()
        .filter(|(k, _)| is_shell_identifier(k))
        .filter_map(|(k, v)| {
            let value = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((k.clone(), value))
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
/// It sets up the environment the session gets (`SKULK_*`, the worktree's
/// `.env`, then the settings `env`), and starts the command as a background
/// job under `set -m`, so the command and its children form their own
/// process group that the watchdog and the signal trap can kill as a whole.
/// bash, not `sh`: dash ignores `set -m` without a terminal.
///
/// In capture mode the command's streams go to temp files and the script
/// prints a [`RESULT`] line (`exit timed_out stdout_bytes stderr_bytes`)
/// followed by hex dumps of the capped streams, which survive the lossy UTF-8
/// conversion and trimming of `Ssh::run`. Otherwise the command writes to the
/// script's own streams and the script exits with the command's code.
fn exec_script(
    agent: &Agent,
    worktree: &str,
    env: &[(String, String)],
    req: &ExecRequest,
    capture: bool,
) -> String {
    let wt = shell_quote_path(worktree);
    let name = agent.name();
    let session = agent.session_name();
    let mut s = format!(
        "cd {wt} || exit 1; \
         export SKULK_AGENT_NAME='{name}' SKULK_SESSION='{session}' SKULK_BRANCH='{session}' SKULK_WORKTREE=\"$PWD\"; \
         [ -f .env ] && {{ set -a; . ./.env; set +a; }} >&2; "
    );
    for (k, v) in env {
        let _ = write!(s, "export {k}='{}'; ", shell_escape(v));
    }
    let redirect = if capture {
        " >\"$_st/o\" 2>\"$_st/e\""
    } else {
        ""
    };
    let _ = write!(
        s,
        "_st=$(mktemp -d \"${{TMPDIR:-/tmp}}/skulk-exec.XXXXXX\") || exit 1; _sp=; \
         trap '[ -n \"$_sp\" ] && kill -TERM -\"$_sp\" 2>/dev/null; rm -rf \"$_st\"; exit 130' INT TERM HUP; \
         set -m; \
         {cmd} </dev/null{redirect} & _sp=$!; ",
        cmd = quote_argv(req.argv),
    );
    if let Some(secs) = req.timeout_secs {
        let _ = write!(
            s,
            "( sleep {secs}; kill -TERM -\"$_sp\" 2>/dev/null || exit 0; : >\"$_st/x\"; \
             sleep {KILL_GRACE_SECS}; kill -KILL -\"$_sp\" 2>/dev/null ) >/dev/null 2>&1 & _sw=$!; "
        );
    }
    s.push_str("wait \"$_sp\"; _sc=$?; ");
    if req.timeout_secs.is_some() {
        s.push_str("kill -TERM -\"$_sw\" 2>/dev/null; ");
    }
    let _ = write!(
        s,
        "set +m; _sx=0; [ -f \"$_st/x\" ] && {{ _sc={TIMEOUT_EXIT_CODE}; _sx=1; }}; "
    );
    if capture {
        let _ = write!(
            s,
            "printf '{RESULT} %s %s %s %s\\n' \"$_sc\" \"$_sx\" $(wc -c <\"$_st/o\") $(wc -c <\"$_st/e\"); \
             head -c {OUTPUT_CAP_BYTES} \"$_st/o\" | od -An -v -tx1; \
             echo {STDERR_MARK}; \
             head -c {OUTPUT_CAP_BYTES} \"$_st/e\" | od -An -v -tx1; \
             rm -rf \"$_st\"; exit 0"
        );
    } else {
        let _ = write!(
            s,
            "[ \"$_sx\" = 1 ] && echo \"skulk exec: timed out after {secs}s; command killed\" >&2; \
             rm -rf \"$_st\"; exit \"$_sc\"",
            secs = req.timeout_secs.unwrap_or(0),
        );
    }
    format!("bash -c '{}'", shell_escape(&s))
}

/// Decode the capture script's output (see [`exec_script`]).
fn parse_captured(out: &str) -> Result<Captured, SkulkError> {
    let malformed = || SkulkError::SshFailed(format!("Unexpected output from skulk exec: {out}"));
    let mut lines = out.lines().skip_while(|l| !l.starts_with(RESULT));
    let header = lines.next().ok_or_else(malformed)?;
    let fields: Vec<&str> = header.split_whitespace().skip(1).collect();
    let [code, timed_out, stdout_total, stderr_total] = fields[..] else {
        return Err(malformed());
    };
    let exit_code = code.parse().map_err(|_| malformed())?;
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
        exit_code,
        timed_out: timed_out == "1",
        stdout,
        stdout_total,
        stderr,
        stderr_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, test_config};

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
        let ssh = MockSsh::new(vec![
            Ok(READY.to_string()),
            Ok(format!("{RESULT} 3 0 2 0\n 68 69\n{STDERR_MARK}")),
        ]);
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
    }

    #[test]
    fn human_mode_streams_through_interactive() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok(READY.to_string())]);
        let args = argv(&["echo", "hi"]);
        let req = ExecRequest {
            name: "a",
            argv: &args,
            timeout_secs: None,
        };

        cmd_exec(&ssh, &req, &cfg).expect("exec succeeds");

        let calls = ssh.calls();
        assert_eq!(calls.len(), 2);
        assert!(!calls[1].contains(RESULT));
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

        let without = exec_script(&agent, "/w", &[], &req, true);
        req.timeout_secs = Some(7);
        let with = exec_script(&agent, "/w", &[], &req, true);

        assert!(!without.contains("sleep"));
        assert!(with.contains("sleep 7"));
        assert!(with.contains("kill -KILL"));
    }

    #[test]
    fn parse_captured_decodes_streams_and_flags() {
        let out = format!("noise\n{RESULT} 124 1 5 1\n 68 69\n 0a\n{STDERR_MARK}\n 21");

        let captured = parse_captured(&out).expect("parses");

        assert_eq!(
            captured,
            Captured {
                exit_code: 124,
                timed_out: true,
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
            exit_code: 0,
            timed_out: false,
            stdout: b"ab".to_vec(),
            stdout_total: 10,
            stderr: Vec::new(),
            stderr_total: 0,
        };

        let v = json_exec_result("a", &c);

        assert_eq!(v["stdout_truncated"], true);
        assert_eq!(v["stderr_truncated"], false);
        assert_eq!(v["stdout"], "ab");
    }
}
