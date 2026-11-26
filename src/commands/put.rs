use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::Agent;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::shell_escape;

/// Where `skulk put` drops a file when no destination is given, relative to
/// the agent's worktree. The file is also staged here before it is placed.
pub(crate) const DEFAULT_PUT_DIR: &str = "tmp/bzworkspace-uploads";

/// Prefix of the line the remote scripts print on stderr when they refuse a
/// destination, so the refusal is reported as a validation error.
const REFUSAL: &str = "skulk-put-refused: ";

/// Printed by the prepare script once the upload directory is ready.
const READY: &str = "skulk-put-ready";

/// Where the file lands inside the worktree.
#[derive(Debug, PartialEq, Eq)]
enum Destination {
    /// `DEFAULT_PUT_DIR/<stem><ext>`, numbered `<stem>-1<ext>`, `<stem>-2<ext>`…
    /// when the name is taken, so a path already handed out keeps its file.
    Default { stem: String, ext: String },
    /// A path the caller chose; an existing file there is overwritten.
    Explicit(String),
}

/// Copy `local_file` into agent `name`'s worktree and print the absolute
/// physical remote path it landed at.
///
/// Three round trips: prepare the upload directory (checking it resolves
/// inside the worktree), copy the file to a staging name in it (the transfer
/// path is not shell-quoted, so it must be shell-safe), then move it to its
/// destination with a quoted script that refuses any path leaving the
/// worktree through a symlink.
pub(crate) fn cmd_put(
    ssh: &impl Ssh,
    name: &str,
    local_file: &Path,
    dest: Option<&str>,
    cfg: &Config,
) -> Result<(), SkulkError> {
    let agent = Agent::resolve(name, cfg)?;
    let destination = match dest {
        Some(d) => Destination::Explicit(validate_dest(d)?.to_string()),
        None => default_destination(local_file)?,
    };
    if !local_file.is_file() {
        return Err(SkulkError::Validation(format!(
            "Local file '{}' does not exist or is not a regular file.",
            local_file.display()
        )));
    }

    let worktree = agent.worktree_path(cfg);
    let prepared = ssh
        .run(&prepare_script(&worktree))
        .map_err(|e| script_error(&agent, e))?;
    if prepared.trim() != READY {
        return Err(SkulkError::NotFound(format!(
            "Agent '{name}' has no worktree on {}. Check agents with `skulk list`.",
            cfg.host
        )));
    }

    let staging = format!("{DEFAULT_PUT_DIR}/{}", staging_name());
    let staging_path = format!("{worktree}/{staging}");
    ssh.upload_file(local_file, &staging_path)
        .map_err(|e| agent.classify(e))?;

    let placed = ssh.run(&place_script(&worktree, &staging, &destination));
    let remote_path = match placed {
        Ok(out) if !out.trim().is_empty() => out.trim().to_string(),
        Ok(_) => {
            let _ = ssh.run(&format!("rm -f {staging_path}"));
            return Err(SkulkError::SshFailed(format!(
                "Copied the file to {}, but could not determine where it landed.",
                cfg.host
            )));
        }
        Err(e) => {
            let _ = ssh.run(&format!("rm -f {staging_path}"));
            return Err(script_error(&agent, e));
        }
    };

    if cfg.output_format == OutputFormat::Json {
        emit_json(&json_put_result(name, &remote_path))?;
    } else {
        println!("{remote_path}");
    }
    Ok(())
}

/// The `--json` success payload.
pub(crate) fn json_put_result(name: &str, path: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "agent": name,
        "path": path,
    })
}

/// Map a failed remote script: a refusal it printed becomes a validation
/// error; anything else is an SSH failure on behalf of the agent.
fn script_error(agent: &Agent, err: SkulkError) -> SkulkError {
    if let SkulkError::SshFailed(stderr) = &err
        && let Some(line) = stderr.lines().find_map(|l| l.strip_prefix(REFUSAL))
    {
        return SkulkError::Validation(line.to_string());
    }
    agent.classify(err)
}

/// Accept a destination relative to the worktree, without `..`.
///
/// Symlinks are checked on the remote by [`place_script`].
fn validate_dest(dest: &str) -> Result<&str, SkulkError> {
    let reject = |why: &str| {
        Err(SkulkError::Validation(format!(
            "Invalid destination '{dest}': {why}"
        )))
    };
    if dest.is_empty() {
        return reject("it is empty.");
    }
    if dest.chars().any(|c| c == '\0' || c == '\n' || c == '\r') {
        return reject("it contains a control character.");
    }
    if dest.ends_with('/') {
        return reject("it must name a file, not a directory.");
    }
    let path = Path::new(dest);
    if path.is_absolute() {
        return reject("it must be relative to the agent's worktree.");
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return reject("it must not contain '..'.");
    }
    if !path.components().any(|c| matches!(c, Component::Normal(_))) {
        return reject("it must name a file.");
    }
    Ok(dest)
}

/// The default destination, named after `local_file` with every character
/// outside `[A-Za-z0-9._-]` replaced by `-`, so the printed path can be typed
/// into a terminal without quoting.
fn default_destination(local_file: &Path) -> Result<Destination, SkulkError> {
    let invalid = || {
        SkulkError::Validation(format!(
            "Local file '{}' has no usable file name.",
            local_file.display()
        ))
    };
    let stem = local_file.file_stem().ok_or_else(invalid)?;
    let stem = sanitize(&stem.to_string_lossy());
    let ext = local_file.extension().map_or_else(String::new, |e| {
        format!(".{}", sanitize(&e.to_string_lossy()))
    });
    Ok(Destination::Default { stem, ext })
}

fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A staging file name unique to this process and moment.
fn staging_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(".skulk-put-{}-{nanos}", std::process::id())
}

/// Shell prologue shared by both scripts: enter the worktree, record its
/// physical path as `$W`, and define `refuse` and `inside`.
fn prologue(worktree: &str) -> String {
    format!(
        "cd {worktree} 2>/dev/null || exit 0; W=$(pwd -P) || exit 1; \
         refuse() {{ echo \"{REFUSAL}$1\" >&2; exit 1; }}; \
         inside() {{ case \"$1/\" in \"$W\"/*) return 0;; esac; return 1; }}; "
    )
}

/// Shell snippet that sets `$D` to the physical path of the upload
/// directory, refusing when `tmp` or the directory itself is a symlink or
/// when it resolves outside the worktree.
fn upload_dir_check() -> String {
    format!(
        "for l in tmp {DEFAULT_PUT_DIR}; do \
         [ -L \"$l\" ] && refuse \"$l in the agent's worktree is a symlink.\"; \
         done; \
         D=$(cd -P {DEFAULT_PUT_DIR} && pwd -P) || exit 1; \
         inside \"$D\" || refuse \"{DEFAULT_PUT_DIR} resolves outside the agent's worktree.\"; "
    )
}

/// Shell script that creates the upload directory inside the worktree and
/// prints [`READY`], or prints nothing when the worktree does not exist.
fn prepare_script(worktree: &str) -> String {
    format!(
        "{}{}mkdir -p {DEFAULT_PUT_DIR} || exit 1; {}echo {READY}",
        prologue(worktree),
        // Refuse before mkdir -p, so it never creates directories through a
        // symlinked `tmp`.
        "[ -L tmp ] && refuse \"tmp in the agent's worktree is a symlink.\"; ",
        upload_dir_check(),
    )
}

/// Shell script that moves `staging` (relative to the worktree) to its
/// destination and prints the destination's physical absolute path.
///
/// For the default destination, `ln` claims the name: it fails when the name
/// exists, so two concurrent puts never take the same one. The `-e`/`-L`
/// checks come first because `ln` into an existing directory would succeed
/// by linking inside it.
///
/// For an explicit destination, each parent directory is created (if
/// missing) and resolved one component at a time, so neither `mkdir` nor
/// `mv` ever follows a symlink out of the worktree.
fn place_script(worktree: &str, staging: &str, destination: &Destination) -> String {
    let prologue = prologue(worktree);
    let staging = shell_escape(staging);
    match destination {
        Destination::Default { stem, ext } => format!(
            "{prologue}s='{staging}'; {check}\
             n=0; p=\"$D/\"'{stem}{ext}'; \
             while [ -e \"$p\" ] || [ -L \"$p\" ] || ! ln \"$s\" \"$p\" 2>/dev/null; do \
             if [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; then echo \"cannot create $p\" >&2; exit 1; fi; \
             n=$((n+1)); p=\"$D/\"'{stem}-'\"$n\"'{ext}'; \
             done; rm -f \"$s\"; printf '%s\\n' \"$p\"",
            check = upload_dir_check(),
            stem = shell_escape(stem),
            ext = shell_escape(ext),
        ),
        Destination::Explicit(dest) => {
            let parts: Vec<String> = Path::new(dest)
                .components()
                .filter_map(|c| match c {
                    Component::Normal(part) => {
                        Some(format!("'{}'", shell_escape(&part.to_string_lossy())))
                    }
                    _ => None,
                })
                .collect();
            let Some((file, dirs)) = parts.split_last() else {
                return format!("{prologue}refuse \"destination names no file.\"");
            };
            let dirs = dirs.join(" ");
            format!(
                "{prologue}s='{staging}'; cur=\"$W\"; \
                 for c in {dirs}; do \
                 next=\"$cur/$c\"; \
                 if [ ! -e \"$next\" ] && [ ! -L \"$next\" ]; then mkdir \"$next\" || exit 1; fi; \
                 next=$(cd -P \"$next\" 2>/dev/null && pwd -P) || refuse \"$cur/$c is not a directory.\"; \
                 inside \"$next\" || refuse \"$cur/$c leads outside the agent's worktree.\"; \
                 cur=\"$next\"; \
                 done; \
                 t=\"$cur/\"{file}; \
                 [ -L \"$t\" ] && refuse \"$t is a symlink.\"; \
                 [ -d \"$t\" ] && refuse \"$t is a directory.\"; \
                 mv -f \"$s\" \"$t\" || exit 1; printf '%s\\n' \"$t\""
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        MockSsh, assert_err, ssh_err, test_config, test_config_json, unique_temp_path,
    };

    fn local_file(name: &str) -> std::path::PathBuf {
        let dir = unique_temp_path("skulk_put");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join(name);
        std::fs::write(&file, b"data").expect("write temp file");
        file
    }

    // Wrapped so it sits next to `ssh_err` inside `MockSsh::new(vec![...])`.
    #[allow(clippy::unnecessary_wraps)]
    fn present() -> Result<String, SkulkError> {
        Ok(format!("{READY}\n"))
    }

    /// Run `script` with `sh -c` inside `dir` standing in for the worktree.
    fn run_in(dir: &Path, script: &str) -> std::process::Output {
        let script = script.replacen("cd WT", &format!("cd '{}'", dir.display()), 1);
        std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .expect("run sh")
    }

    // ── cmd_put ─────────────────────────────────────────────────────────

    #[test]
    fn uploads_to_staging_then_places_file() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![present(), Ok("/home/u/wt/x\n".into())]);

        let result = cmd_put(&ssh, "my-agent", &file, None, &test_config());

        assert!(result.is_ok(), "{result:?}");
        let calls = ssh.calls();
        assert_eq!(calls.len(), 3);
        assert!(
            calls[0].starts_with("cd ~/test-project-worktrees/skulk-my-agent "),
            "{}",
            calls[0]
        );
        assert!(
            calls[1].starts_with(&format!(
                "UPLOAD {}:~/test-project-worktrees/skulk-my-agent/tmp/bzworkspace-uploads/.skulk-put-",
                file.display()
            )),
            "{}",
            calls[1]
        );
        assert!(calls[2].contains("tmp/bzworkspace-uploads"), "{}", calls[2]);
    }

    #[test]
    fn json_put_result_reports_agent_and_path() {
        let value = json_put_result("my-agent", "/abs/image.png");

        assert_eq!(
            value,
            serde_json::json!({"ok": true, "agent": "my-agent", "path": "/abs/image.png"})
        );
    }

    #[test]
    fn classifies_probe_failure_instead_of_reporting_missing_worktree() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![ssh_err(
            "ssh: connect to host testhost: Connection refused",
        )]);

        let result = cmd_put(&ssh, "my-agent", &file, None, &test_config());

        assert_err!(result, SkulkError::Diagnostic { message, .. } => {
            assert!(message.contains("refused"), "{message}");
        });
    }

    #[test]
    fn reports_remote_refusal_as_validation_error() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![ssh_err(&format!(
            "{REFUSAL}tmp in the agent's worktree is a symlink."
        ))]);

        let result = cmd_put(&ssh, "my-agent", &file, None, &test_config_json());

        assert_err!(result, SkulkError::Validation(msg) => {
            assert_eq!(msg, "tmp in the agent's worktree is a symlink.");
        });
    }

    #[test]
    fn returns_not_found_when_agent_has_no_worktree() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![Ok(String::new())]);

        let result = cmd_put(&ssh, "ghost", &file, None, &test_config());

        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("ghost"), "{msg}");
        });
        assert_eq!(ssh.calls().len(), 1, "nothing is uploaded");
    }

    #[test]
    fn returns_error_when_local_file_is_missing() {
        let ssh = MockSsh::new(vec![]);

        let result = cmd_put(
            &ssh,
            "my-agent",
            Path::new("/nonexistent/skulk/image.png"),
            None,
            &test_config(),
        );

        assert_err!(result, SkulkError::Validation(msg) => {
            assert!(msg.contains("/nonexistent/skulk/image.png"), "{msg}");
        });
        assert_eq!(ssh.calls(), Vec::<String>::new());
    }

    #[test]
    fn returns_error_when_agent_name_is_invalid() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![]);

        let result = cmd_put(&ssh, "Bad Name", &file, None, &test_config());

        assert_err!(result, SkulkError::Validation(_) => {});
        assert_eq!(ssh.calls(), Vec::<String>::new());
    }

    #[test]
    fn removes_staging_file_when_placing_fails() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![
            present(),
            ssh_err("mkdir: Permission denied"),
            Ok(String::new()),
        ]);

        let result = cmd_put(&ssh, "my-agent", &file, None, &test_config());

        assert!(result.is_err());
        let calls = ssh.calls();
        assert!(
            calls[3].starts_with(
                "rm -f ~/test-project-worktrees/skulk-my-agent/tmp/bzworkspace-uploads/.skulk-put-"
            ),
            "{}",
            calls[3]
        );
    }

    #[test]
    fn stops_when_transfer_fails() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![present()])
            .with_upload_responses(vec![Err(SkulkError::SshFailed("scp: broken".into()))]);

        let result = cmd_put(&ssh, "my-agent", &file, None, &test_config());

        assert!(result.is_err());
        assert_eq!(ssh.calls().len(), 2, "no placing after a failed transfer");
    }

    // ── validate_dest ───────────────────────────────────────────────────

    #[test]
    fn accepts_nested_relative_destination() {
        assert_eq!(
            validate_dest("docs/my notes.txt").ok(),
            Some("docs/my notes.txt")
        );
    }

    #[test]
    fn rejects_destinations_outside_or_not_naming_a_file() {
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "dir/", ".", "a\nb"] {
            assert!(validate_dest(bad).is_err(), "accepted {bad:?}");
        }
    }

    // ── default_destination ─────────────────────────────────────────────

    #[test]
    fn default_destination_keeps_a_plain_name() {
        assert_eq!(
            default_destination(Path::new("/x/image.png")).ok(),
            Some(Destination::Default {
                stem: "image".into(),
                ext: ".png".into()
            })
        );
    }

    #[test]
    fn default_destination_replaces_characters_unsafe_to_type() {
        assert_eq!(
            default_destination(Path::new("/x/Screen Shot (1)'s.PNG")).ok(),
            Some(Destination::Default {
                stem: "Screen-Shot--1--s".into(),
                ext: ".PNG".into()
            })
        );
    }

    #[test]
    fn default_destination_handles_names_without_extension() {
        assert_eq!(
            default_destination(Path::new("/x/notes")).ok(),
            Some(Destination::Default {
                stem: "notes".into(),
                ext: String::new()
            })
        );
    }

    // ── prepare_script / place_script, run for real ─────────────────────

    const STAGE: &str = "tmp/bzworkspace-uploads/.stage";

    /// A fresh worktree stand-in, with its physical path.
    fn worktree() -> std::path::PathBuf {
        let wt = unique_temp_path("skulk_put_wt");
        std::fs::create_dir_all(&wt).expect("mkdir");
        wt.canonicalize().expect("canonicalize")
    }

    /// A worktree whose upload dir holds a staged file containing `new`.
    fn staged_worktree() -> std::path::PathBuf {
        let wt = worktree();
        let out = run_in(&wt, &prepare_script("WT"));
        assert!(out.status.success(), "{out:?}");
        std::fs::write(wt.join(STAGE), b"new").expect("stage");
        wt
    }

    fn stderr(out: &std::process::Output) -> String {
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn stdout(out: &std::process::Output) -> String {
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("symlink");
    }

    #[test]
    fn prepare_script_creates_upload_dir_and_reports_ready() {
        let wt = worktree();

        let out = run_in(&wt, &prepare_script("WT"));

        assert_eq!(stdout(&out), READY);
        assert!(wt.join(DEFAULT_PUT_DIR).is_dir());
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn prepare_script_prints_nothing_when_worktree_is_missing() {
        let wt = unique_temp_path("skulk_put_missing");

        let out = run_in(&wt, &prepare_script("WT"));

        assert!(out.status.success(), "{out:?}");
        assert_eq!(stdout(&out), "");
    }

    #[test]
    fn prepare_script_refuses_symlinked_tmp_without_writing_outside() {
        let wt = worktree();
        let outside = worktree();
        symlink(&outside, &wt.join("tmp"));

        let out = run_in(&wt, &prepare_script("WT"));

        assert!(!out.status.success());
        assert!(stderr(&out).contains(REFUSAL), "{}", stderr(&out));
        assert!(!outside.join("bzworkspace-uploads").exists());
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn prepare_script_refuses_symlinked_upload_dir() {
        let wt = worktree();
        let outside = worktree();
        std::fs::create_dir_all(wt.join("tmp")).expect("mkdir");
        symlink(&outside, &wt.join(DEFAULT_PUT_DIR));

        let out = run_in(&wt, &prepare_script("WT"));

        assert!(!out.status.success());
        assert!(stderr(&out).contains(REFUSAL), "{}", stderr(&out));
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn place_script_numbers_taken_default_names_and_skips_directories() {
        let wt = staged_worktree();
        std::fs::create_dir_all(wt.join("tmp/bzworkspace-uploads/a-1.png")).expect("mkdir");
        std::fs::write(wt.join("tmp/bzworkspace-uploads/a.png"), b"old").expect("seed");
        let dest = Destination::Default {
            stem: "a".into(),
            ext: ".png".into(),
        };

        let out = run_in(&wt, &place_script("WT", STAGE, &dest));

        assert!(out.status.success(), "{out:?}");
        let expected = wt.join("tmp/bzworkspace-uploads/a-2.png");
        assert_eq!(stdout(&out), expected.to_string_lossy());
        assert_eq!(
            std::fs::read(wt.join("tmp/bzworkspace-uploads/a.png")).expect("a"),
            b"old"
        );
        assert_eq!(std::fs::read(&expected).expect("a-2"), b"new");
        assert!(!wt.join(STAGE).exists(), "staging file removed");
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn place_script_refuses_default_dest_when_tmp_is_symlinked_outside() {
        let wt = staged_worktree();
        let outside = worktree();
        std::fs::rename(wt.join("tmp"), outside.join("tmp")).expect("move tmp out");
        symlink(&outside.join("tmp"), &wt.join("tmp"));
        let dest = Destination::Default {
            stem: "a".into(),
            ext: ".png".into(),
        };

        let out = run_in(&wt, &place_script("WT", STAGE, &dest));

        assert!(!out.status.success());
        assert!(stderr(&out).contains(REFUSAL), "{}", stderr(&out));
        assert!(!outside.join("tmp/bzworkspace-uploads/a.png").exists());
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn place_script_quotes_explicit_destination_and_prints_clean_path() {
        let wt = staged_worktree();
        let dest = Destination::Explicit("./a dir//it's $HOME.txt".into());

        let out = run_in(&wt, &place_script("WT", STAGE, &dest));

        assert!(out.status.success(), "{out:?}");
        let expected = wt.join("a dir/it's $HOME.txt");
        assert_eq!(stdout(&out), expected.to_string_lossy());
        assert_eq!(std::fs::read(&expected).expect("placed"), b"new");
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn place_script_follows_symlinks_that_stay_inside_the_worktree() {
        let wt = staged_worktree();
        std::fs::create_dir_all(wt.join("real")).expect("mkdir");
        symlink(&wt.join("real"), &wt.join("alias"));
        let dest = Destination::Explicit("alias/x.txt".into());

        let out = run_in(&wt, &place_script("WT", STAGE, &dest));

        assert!(out.status.success(), "{out:?}");
        assert_eq!(stdout(&out), wt.join("real/x.txt").to_string_lossy());
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn place_script_refuses_explicit_dest_under_symlink_leading_outside() {
        let wt = staged_worktree();
        let outside = worktree();
        symlink(&outside, &wt.join("docs"));
        let dest = Destination::Explicit("docs/new/.bashrc".into());

        let out = run_in(&wt, &place_script("WT", STAGE, &dest));

        assert!(!out.status.success());
        assert!(stderr(&out).contains(REFUSAL), "{}", stderr(&out));
        assert_eq!(
            std::fs::read_dir(&outside).expect("read outside").count(),
            0,
            "nothing written outside the worktree"
        );
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn place_script_refuses_explicit_dest_that_is_a_symlink() {
        let wt = staged_worktree();
        let outside = worktree();
        std::fs::write(outside.join("target"), b"keep").expect("seed");
        symlink(&outside.join("target"), &wt.join("link.txt"));
        let dest = Destination::Explicit("link.txt".into());

        let out = run_in(&wt, &place_script("WT", STAGE, &dest));

        assert!(!out.status.success());
        assert!(stderr(&out).contains(REFUSAL), "{}", stderr(&out));
        assert_eq!(
            std::fs::read(outside.join("target")).expect("target"),
            b"keep"
        );
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn place_script_refuses_explicit_destination_that_is_a_directory() {
        let wt = staged_worktree();
        std::fs::create_dir_all(wt.join("taken")).expect("mkdir");
        let dest = Destination::Explicit("taken".into());

        let out = run_in(&wt, &place_script("WT", STAGE, &dest));

        assert!(!out.status.success());
        assert!(wt.join("taken").is_dir());
        let _ = std::fs::remove_dir_all(&wt);
    }
}
