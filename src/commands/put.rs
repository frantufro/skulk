use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::Agent;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::shell_escape;

/// Where `skulk put` drops a file when no destination is given, relative to
/// the agent's worktree.
pub(crate) const DEFAULT_PUT_DIR: &str = "tmp/bzworkspace-uploads";

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
/// remote path it landed at.
///
/// The file is first copied to a staging name in the worktree root (the
/// transfer path is not shell-quoted, so it must be shell-safe), then moved
/// to its destination by a quoted script that also picks the free name.
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
    let probe = ssh
        .run(&format!("test -d {worktree} && echo present"))
        .unwrap_or_default();
    if probe.trim() != "present" {
        return Err(SkulkError::NotFound(format!(
            "Agent '{name}' has no worktree on {}. Check agents with `skulk list`.",
            cfg.host
        )));
    }

    let staging = staging_name();
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
            return Err(agent.classify(e));
        }
    };

    if cfg.output_format == OutputFormat::Json {
        emit_json(&serde_json::json!({
            "ok": true,
            "agent": name,
            "path": remote_path,
        }));
    } else {
        println!("{remote_path}");
    }
    Ok(())
}

/// Accept a destination relative to the worktree that stays inside it.
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

/// A worktree-root file name unique to this process and moment.
fn staging_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(".skulk-put-{}-{nanos}", std::process::id())
}

/// Shell script that moves `staging` (in the worktree root) to its
/// destination and prints the destination's absolute path.
///
/// For the default destination, `ln` claims the name: it fails when the name
/// exists, so two concurrent puts never take the same one. The `-e`/`-L`
/// checks come first because `ln` into an existing directory would succeed
/// by linking inside it.
fn place_script(worktree: &str, staging: &str, destination: &Destination) -> String {
    match destination {
        Destination::Default { stem, ext } => format!(
            "cd {worktree} && s='{staging}' && d='{DEFAULT_PUT_DIR}' && mkdir -p \"$d\" && \
             n=0 && p=\"$d/\"'{stem}{ext}' && \
             while [ -e \"$p\" ] || [ -L \"$p\" ] || ! ln \"$s\" \"$p\" 2>/dev/null; do \
             if [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; then echo \"cannot create $p\" >&2; exit 1; fi; \
             n=$((n+1)); p=\"$d/\"'{stem}-'\"$n\"'{ext}'; \
             done && rm -f \"$s\" && printf '%s\\n' \"$PWD/$p\"",
            stem = shell_escape(stem),
            ext = shell_escape(ext),
        ),
        Destination::Explicit(dest) => format!(
            "cd {worktree} && s='{staging}' && p='{dest}' && \
             if [ -d \"$p\" ]; then echo \"$p is a directory\" >&2; exit 1; fi && \
             mkdir -p \"$(dirname \"$p\")\" && mv -f \"$s\" \"$p\" && \
             printf '%s\\n' \"$PWD/$p\"",
            dest = shell_escape(dest),
        ),
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
        Ok("present\n".into())
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
        assert_eq!(
            calls[0],
            "test -d ~/test-project-worktrees/skulk-my-agent && echo present"
        );
        assert!(
            calls[1].starts_with(&format!(
                "UPLOAD {}:~/test-project-worktrees/skulk-my-agent/.skulk-put-",
                file.display()
            )),
            "{}",
            calls[1]
        );
        assert!(calls[2].contains("tmp/bzworkspace-uploads"), "{}", calls[2]);
    }

    #[test]
    fn prints_json_result_in_json_mode() {
        let file = local_file("image.png");
        let ssh = MockSsh::new(vec![present(), Ok("/abs/image.png\n".into())]);

        let result = cmd_put(&ssh, "my-agent", &file, None, &test_config_json());

        assert!(result.is_ok(), "{result:?}");
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
            calls[3].starts_with("rm -f ~/test-project-worktrees/skulk-my-agent/.skulk-put-"),
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

    // ── place_script, run for real ──────────────────────────────────────

    #[test]
    fn place_script_numbers_taken_default_names_and_skips_directories() {
        let wt = unique_temp_path("skulk_put_wt");
        std::fs::create_dir_all(wt.join("tmp/bzworkspace-uploads/a-1.png")).expect("mkdir");
        std::fs::write(wt.join("tmp/bzworkspace-uploads/a.png"), b"old").expect("seed");
        std::fs::write(wt.join(".stage"), b"new").expect("stage");
        let dest = Destination::Default {
            stem: "a".into(),
            ext: ".png".into(),
        };

        let out = run_in(&wt, &place_script("WT", ".stage", &dest));

        assert!(out.status.success(), "{out:?}");
        let printed = String::from_utf8_lossy(&out.stdout);
        assert!(
            printed
                .trim_end()
                .ends_with("/tmp/bzworkspace-uploads/a-2.png"),
            "{printed}"
        );
        assert_eq!(
            std::fs::read(wt.join("tmp/bzworkspace-uploads/a.png")).expect("a"),
            b"old"
        );
        assert_eq!(
            std::fs::read(wt.join("tmp/bzworkspace-uploads/a-2.png")).expect("a-2"),
            b"new"
        );
        assert!(!wt.join(".stage").exists(), "staging file removed");
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn place_script_quotes_explicit_destination() {
        let wt = unique_temp_path("skulk_put_wt");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt.join(".stage"), b"new").expect("stage");
        let dest = Destination::Explicit("a dir/it's $HOME.txt".into());

        let out = run_in(&wt, &place_script("WT", ".stage", &dest));

        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            std::fs::read(wt.join("a dir/it's $HOME.txt")).expect("placed"),
            b"new"
        );
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn place_script_refuses_explicit_destination_that_is_a_directory() {
        let wt = unique_temp_path("skulk_put_wt");
        std::fs::create_dir_all(wt.join("taken")).expect("mkdir");
        std::fs::write(wt.join(".stage"), b"new").expect("stage");
        let dest = Destination::Explicit("taken".into());

        let out = run_in(&wt, &place_script("WT", ".stage", &dest));

        assert!(!out.status.success());
        assert!(wt.join("taken").is_dir());
        let _ = std::fs::remove_dir_all(&wt);
    }
}
