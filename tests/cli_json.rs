//! End-to-end checks of the `--json` contract through the real binary.
//!
//! Every test runs skulk in localhost mode against throwaway git repos, named
//! with `--config`. `TMUX_TMPDIR`, `HOME` and `SHELL` are pinned for each test
//! and `TMUX` is unset, so every tmux call reaches a private server under the
//! test's temp dir and never the default one where real agents run. The
//! harness is a stub script at an absolute path, so no real agent starts.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A name no other test (or concurrent test run) uses.
fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Whether tmux can run here. In CI a missing tmux fails the test instead of
/// letting it pass without checking anything.
fn tmux_available() -> bool {
    let installed = Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        installed || std::env::var_os("CI").is_none(),
        "tmux is required for this test in CI"
    );
    if !installed {
        eprintln!("skipping: tmux is not installed");
    }
    installed
}

struct Project {
    root: PathBuf,
}

impl Project {
    /// A project whose base clone tracks a bare `origin` with one commit on `main`.
    fn with_base_clone() -> Self {
        let project = Self::empty();
        let origin = project.root.join("origin.git");
        let base = project.base_path();
        git(
            &project.root,
            &["init", "-q", "--bare", "-b", "main", path(&origin)],
        );
        git(&project.root, &["clone", "-q", path(&origin), path(&base)]);
        git(&base, &["checkout", "-q", "-b", "main"]);
        git(&base, &["commit", "-q", "--allow-empty", "-m", "initial"]);
        git(&base, &["push", "-q", "-u", "origin", "main"]);
        project
    }

    /// A project whose config points at a base clone that does not exist.
    fn empty() -> Self {
        // Short on purpose: tmux socket paths are limited to about 100 bytes.
        let root = PathBuf::from(format!("/tmp/{}", unique("skulk-cli")));
        let _ = std::fs::remove_dir_all(&root);
        for dir in [".skulk", "home", "tmux", "bin"] {
            std::fs::create_dir_all(root.join(dir)).expect("create project dirs");
        }
        let project = Self { root };
        let harness = project.root.join("bin/harness");
        std::fs::write(&harness, "#!/bin/sh\nexec sleep 600\n").expect("write stub harness");
        make_executable(&harness);
        std::fs::write(
            project.root.join(".skulk/config.toml"),
            format!(
                "host = \"localhost\"\n\
                 session_prefix = \"skulk-\"\n\
                 base_path = \"{}\"\n\
                 worktree_base = \"{}\"\n\
                 harness = \"{}\"\n",
                project.base_path().display(),
                project.root.join("worktrees").display(),
                harness.display(),
            ),
        )
        .expect("write config");
        project
    }

    fn base_path(&self) -> PathBuf {
        self.root.join("base")
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Run `skulk --json --config <project> <args>` from an unrelated cwd.
    fn skulk(&self, args: &[&str]) -> Output {
        self.skulk_in(&std::env::temp_dir(), args)
    }

    /// Run `skulk --json --config <project> <args>` from `cwd`.
    fn skulk_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_skulk"))
            .args(["--json", "--config", path(&self.root)])
            .args(args)
            .current_dir(cwd)
            .env("SKULK_NO_UPDATE_CHECK", "1")
            .env("HOME", self.home())
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", self.root.join("tmux"))
            .env_remove("TMUX")
            .output()
            .expect("run skulk")
    }

    /// A tmux command bound to this project's private server.
    fn private_tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.env("TMUX_TMPDIR", self.root.join("tmux"))
            .env("HOME", self.home())
            .env("SHELL", "/bin/sh")
            .env_remove("TMUX");
        cmd
    }

    fn has_session(&self, session: &str) -> bool {
        self.private_tmux()
            .args(["has-session", "-t", session])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Stage an archived agent: worktree and branch `skulk-<name>`, no session.
    fn stage_archived_agent(&self, name: &str) {
        let qualified = format!("skulk-{name}");
        let worktree = self.root.join("worktrees").join(&qualified);
        git(
            &self.base_path(),
            &["worktree", "add", "-q", "-b", &qualified, path(&worktree)],
        );
    }

    /// Stage a running agent: an archived agent plus its session on the
    /// private server.
    fn stage_running_agent(&self, name: &str) {
        self.stage_archived_agent(name);
        let qualified = format!("skulk-{name}");
        let worktree = self.root.join("worktrees").join(&qualified);
        let out = self
            .private_tmux()
            .args(["-f", "/dev/null", "new-session", "-d", "-s", &qualified])
            .args(["-c", path(&worktree), "sleep", "600"])
            .output()
            .expect("run tmux");
        assert!(
            out.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            self.has_session(&qualified),
            "agent session must be on the private server before a destructive call"
        );
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.private_tmux().arg("kill-server").output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn make_executable(file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))
        .expect("chmod stub harness");
}

fn path(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=skulk-test",
            "-c",
            "user.email=test@example.com",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Assert `stream` holds exactly one JSON object and return it.
fn single_json_object(stream: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(stream);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected one JSON line, got: {text:?}");
    let value: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
    assert!(value.is_object(), "expected an object, got: {value}");
    assert_eq!(value["v"], 1, "missing schema version: {value}");
    value
}

/// Assert the run failed with one `emit_json_error` object on stderr and
/// nothing on stdout, and return that object.
fn json_failure(out: &Output) -> serde_json::Value {
    assert!(!out.status.success(), "expected failure");
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on failure: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = single_json_object(&out.stderr);
    assert!(err["error"].is_string(), "{err}");
    assert!(err["code"].is_string(), "{err}");
    err
}

/// Assert the run succeeded with exactly one JSON object on stdout.
fn json_success(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    single_json_object(&out.stdout)
}

// ── --config ────────────────────────────────────────────────────────────

#[test]
fn config_flag_with_missing_path_emits_json_config_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_skulk"))
        .args(["--json", "--config", "/nonexistent/skulk-project", "list"])
        .env("SKULK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run skulk");

    let err = json_failure(&out);

    assert_eq!(err["code"], "config");
}

// ── list ────────────────────────────────────────────────────────────────

#[test]
fn list_emits_versioned_object_with_agents_array() {
    let project = Project::with_base_clone();

    let value = json_success(&project.skulk(&["list"]));

    assert_eq!(value["agents"], serde_json::json!([]));
}

// ── push ────────────────────────────────────────────────────────────────

#[test]
fn list_reports_archived_agent_when_no_tmux_server_runs() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("parked");

    let value = json_success(&project.skulk(&["list"]));

    assert_eq!(value["agents"][0]["name"], "parked", "{value}");
    assert_eq!(value["agents"][0]["status"], "stopped", "{value}");
}

#[test]
fn push_emits_one_json_result_on_success() {
    let project = Project::with_base_clone();
    let name = unique("push");
    git(&project.base_path(), &["branch", &format!("skulk-{name}")]);

    let value = json_success(&project.skulk(&["push", &name]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(value["branch"], format!("skulk-{name}"));
}

#[test]
fn push_emits_json_error_when_branch_is_missing() {
    let project = Project::with_base_clone();

    json_failure(&project.skulk(&["push", &unique("ghost")]));
}

// ── pull ────────────────────────────────────────────────────────────────

#[test]
fn pull_emits_one_json_result_on_success() {
    let project = Project::with_base_clone();

    let value = json_success(&project.skulk(&["pull"]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["branch"], "main");
    assert_eq!(value["force"], false);
}

#[test]
fn pull_emits_json_error_when_base_clone_is_missing() {
    let project = Project::empty();

    let err = json_failure(&project.skulk(&["pull"]));

    assert_eq!(err["code"], "validation");
}

// ── doctor ──────────────────────────────────────────────────────────────

#[test]
fn doctor_prints_one_object_and_exits_nonzero_when_a_check_fails() {
    // The base clone does not exist, so that check fails.
    let project = Project::empty();
    // The probe runs `harness --version`, so the stub must return, not sleep.
    std::fs::write(project.root.join("bin/harness"), "#!/bin/sh\necho 1.0.0\n")
        .expect("rewrite stub harness");

    let out = project.skulk(&["doctor"]);

    assert!(!out.status.success(), "a failed check must exit non-zero");
    let report = single_json_object(&out.stdout);
    assert_eq!(report["ok"], false, "{report}");
    let base_clone = report["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["id"] == "base_clone"))
        .expect("base_clone check");
    assert_eq!(base_clone["status"], "fail", "{report}");
    // The error goes to stderr only.
    let err = single_json_object(&out.stderr);
    assert_eq!(err["code"], "validation", "{err}");
}

// ── archive ─────────────────────────────────────────────────────────────

#[test]
fn archive_emits_one_json_result_on_success() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let name = unique("archive");
    project.stage_running_agent(&name);

    let value = json_success(&project.skulk(&["archive", &name, "--reason", "done"]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(value["reason"], "done");
    assert_eq!(value["warnings"], serde_json::json!([]));
    assert!(!project.has_session(&format!("skulk-{name}")));
}

#[test]
fn archive_emits_json_error_when_agent_is_not_running() {
    let project = Project::with_base_clone();

    json_failure(&project.skulk(&["archive", &unique("ghost")]));
}

// ── restart ─────────────────────────────────────────────────────────────

#[test]
fn restart_emits_one_json_result_on_success() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let name = unique("restart");
    project.stage_archived_agent(&name);

    let value = json_success(&project.skulk(&["restart", &name]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(value["session"], format!("skulk-{name}"));
    assert!(project.has_session(&format!("skulk-{name}")));
}

#[test]
fn restart_emits_json_error_when_agent_has_no_worktree() {
    let project = Project::with_base_clone();

    let err = json_failure(&project.skulk(&["restart", &unique("ghost")]));

    assert_eq!(err["code"], "not_found");
}

// ── destroy ─────────────────────────────────────────────────────────────

#[test]
fn destroy_emits_one_json_result_on_success() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let name = unique("destroy");
    project.stage_running_agent(&name);

    let value = json_success(&project.skulk(&["destroy", &name, "--force"]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(
        value["cleaned"],
        serde_json::json!(["session", "worktree", "branch"])
    );
}

#[test]
fn destroy_emits_json_error_when_agent_is_missing() {
    let project = Project::with_base_clone();

    let err = json_failure(&project.skulk(&["destroy", &unique("ghost"), "--force"]));

    assert_eq!(err["code"], "not_found");
}

#[test]
fn destroy_without_force_emits_json_error_instead_of_prompting() {
    let project = Project::with_base_clone();
    let name = unique("noforce");
    project.stage_archived_agent(&name);

    let err = json_failure(&project.skulk(&["destroy", &name]));

    assert_eq!(err["code"], "validation");
    assert!(
        project
            .root
            .join("worktrees")
            .join(format!("skulk-{name}"))
            .is_dir(),
        "nothing may be destroyed without --force"
    );
}

// ── new / replay ────────────────────────────────────────────────────────

#[test]
fn new_emits_one_json_result_on_success() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let name = unique("new");
    let prompt = project.root.join("prompt.txt");
    std::fs::write(&prompt, "do the thing").expect("write prompt");

    let value = json_success(&project.skulk(&["new", &name, "--from", path(&prompt)]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(value["prompt"], "delivered");
    assert_eq!(value["warnings"], serde_json::json!([]));
    assert!(project.has_session(&format!("skulk-{name}")));
}

#[test]
fn replay_emits_one_json_result_on_success() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let source = unique("source");
    let replayed = unique("replayed");
    let prompts = project.home().join(".skulk/prompts");
    std::fs::create_dir_all(&prompts).expect("create prompt store");
    std::fs::write(prompts.join(format!("skulk-{source}.txt")), "do it again")
        .expect("write stored prompt");

    let value = json_success(&project.skulk(&["replay", &source, "--as", &replayed]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], replayed.as_str());
    assert_eq!(value["prompt"], "delivered");
    assert!(project.has_session(&format!("skulk-{replayed}")));
}

#[test]
fn new_emits_json_error_when_base_clone_is_missing() {
    let project = Project::empty();

    let err = json_failure(&project.skulk(&["new", &unique("new")]));

    assert_eq!(err["code"], "validation");
}

// ── put ─────────────────────────────────────────────────────────────────

impl Project {
    /// The agent's worktree as a physical path, which is what `put` prints
    /// (on macOS `/tmp` is a symlink to `/private/tmp`).
    fn worktree(&self, name: &str) -> PathBuf {
        self.root
            .join("worktrees")
            .join(format!("skulk-{name}"))
            .canonicalize()
            .expect("worktree exists")
    }

    /// A local file outside every worktree, holding `contents`.
    fn local_file(&self, file_name: &str, contents: &[u8]) -> PathBuf {
        let dir = self.root.join("local");
        std::fs::create_dir_all(&dir).expect("create local dir");
        let file = dir.join(file_name);
        std::fs::write(&file, contents).expect("write local file");
        file
    }
}

#[test]
fn put_copies_file_into_default_upload_dir_and_reports_absolute_path() {
    let project = Project::with_base_clone();
    let name = unique("put");
    project.stage_archived_agent(&name);
    let image = project.local_file("image.png", b"png bytes");

    let value = json_success(&project.skulk(&["put", &name, path(&image)]));

    let expected = project
        .worktree(&name)
        .join("tmp/bzworkspace-uploads/image.png");
    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(value["path"], path(&expected));
    assert_eq!(
        std::fs::read(&expected).expect("uploaded file"),
        b"png bytes"
    );
}

#[test]
fn put_numbers_the_default_name_when_it_is_taken() {
    let project = Project::with_base_clone();
    let name = unique("put");
    project.stage_archived_agent(&name);
    let first = project.local_file("image.png", b"first");
    json_success(&project.skulk(&["put", &name, path(&first)]));
    let second = project.local_file("image.png", b"second");
    json_success(&project.skulk(&["put", &name, path(&second)]));
    let third = project.local_file("image.png", b"third");

    let value = json_success(&project.skulk(&["put", &name, path(&third)]));

    let dir = project.worktree(&name).join("tmp/bzworkspace-uploads");
    assert_eq!(value["path"], path(&dir.join("image-2.png")));
    assert_eq!(
        std::fs::read(dir.join("image.png")).expect("first"),
        b"first"
    );
    assert_eq!(
        std::fs::read(dir.join("image-1.png")).expect("second"),
        b"second"
    );
    assert_eq!(
        std::fs::read(dir.join("image-2.png")).expect("third"),
        b"third"
    );
}

#[test]
fn put_overwrites_an_explicit_destination() {
    let project = Project::with_base_clone();
    let name = unique("put");
    project.stage_archived_agent(&name);
    let old = project.local_file("old.txt", b"old");
    json_success(&project.skulk(&["put", &name, path(&old), "docs/my notes.txt"]));
    let new = project.local_file("new.txt", b"new");

    let value = json_success(&project.skulk(&["put", &name, path(&new), "docs/my notes.txt"]));

    let expected = project.worktree(&name).join("docs/my notes.txt");
    assert_eq!(value["path"], path(&expected));
    assert_eq!(std::fs::read(&expected).expect("uploaded file"), b"new");
}

#[test]
fn put_leaves_no_staging_file_in_the_worktree() {
    let project = Project::with_base_clone();
    let name = unique("put");
    project.stage_archived_agent(&name);
    let image = project.local_file("image.png", b"png");

    json_success(&project.skulk(&["put", &name, path(&image)]));

    let leftovers: Vec<_> =
        std::fs::read_dir(project.worktree(&name).join("tmp/bzworkspace-uploads"))
            .expect("read worktree")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".skulk-put"))
            .collect();
    assert!(leftovers.is_empty(), "staging files left: {leftovers:?}");
}

#[test]
fn put_emits_json_error_when_agent_has_no_worktree() {
    let project = Project::with_base_clone();
    let image = project.local_file("image.png", b"png");

    let err = json_failure(&project.skulk(&["put", &unique("ghost"), path(&image)]));

    assert_eq!(err["code"], "not_found");
}

#[test]
fn put_emits_json_error_when_destination_escapes_the_worktree() {
    let project = Project::with_base_clone();
    let name = unique("put");
    project.stage_archived_agent(&name);
    let image = project.local_file("image.png", b"png");

    let err = json_failure(&project.skulk(&["put", &name, path(&image), "../outside.png"]));

    assert_eq!(err["code"], "validation");
    assert!(!project.root.join("worktrees/outside.png").exists());
}

#[cfg(unix)]
#[test]
fn put_refuses_explicit_destination_under_a_symlink_leading_outside() {
    let project = Project::with_base_clone();
    let name = unique("put");
    project.stage_archived_agent(&name);
    let outside = project.root.join("outside");
    std::fs::create_dir_all(&outside).expect("create outside dir");
    std::os::unix::fs::symlink(&outside, project.worktree(&name).join("docs")).expect("symlink");
    let file = project.local_file("bashrc", b"evil");

    let err = json_failure(&project.skulk(&["put", &name, path(&file), "docs/.bashrc"]));

    assert_eq!(err["code"], "validation");
    assert_eq!(
        std::fs::read_dir(&outside).expect("read outside").count(),
        0,
        "nothing written outside the worktree"
    );
}

#[cfg(unix)]
#[test]
fn put_refuses_default_destination_when_tmp_is_symlinked_outside() {
    let project = Project::with_base_clone();
    let name = unique("put");
    project.stage_archived_agent(&name);
    let outside = project.root.join("outside");
    std::fs::create_dir_all(&outside).expect("create outside dir");
    std::os::unix::fs::symlink(&outside, project.worktree(&name).join("tmp")).expect("symlink");
    let image = project.local_file("image.png", b"png");

    let err = json_failure(&project.skulk(&["put", &name, path(&image)]));

    assert_eq!(err["code"], "validation");
    assert_eq!(
        std::fs::read_dir(&outside).expect("read outside").count(),
        0,
        "nothing written outside the worktree"
    );
}

// ── get ─────────────────────────────────────────────────────────────────

impl Project {
    /// Stage an agent whose worktree holds `tmp/review-media` with two files.
    fn agent_with_media(&self) -> String {
        let name = unique("get");
        self.stage_archived_agent(&name);
        let media = self.worktree(&name).join("tmp/review-media");
        std::fs::create_dir_all(media.join("walkthrough")).expect("create media dir");
        std::fs::write(media.join("shot-1.png"), b"png!").expect("write shot");
        std::fs::write(media.join("walkthrough/video.webm"), b"webm bytes").expect("write video");
        name
    }

    /// A fresh local directory outside every worktree, as a physical path.
    fn local_dir(&self, dir: &str) -> PathBuf {
        let dir = self.root.join(dir);
        std::fs::create_dir_all(&dir).expect("create local dir");
        dir.canonicalize().expect("canonicalize local dir")
    }
}

#[test]
fn get_copies_a_file_and_reports_it() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let dest = project.local_dir("out").join("shot.png");

    let value =
        json_success(&project.skulk(&["get", &name, "tmp/review-media/shot-1.png", path(&dest)]));

    assert_eq!(value["v"], 1);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(
        value["files"],
        serde_json::json!([{"path": "tmp/review-media/shot-1.png", "bytes": 4}])
    );
    assert_eq!(value["dest"], path(&dest));
    assert_eq!(std::fs::read(&dest).expect("copied file"), b"png!");
}

#[test]
fn get_copies_a_directory_with_recursive() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let dest = project.local_dir("out").join("media");

    let value = json_success(&project.skulk(&[
        "get",
        &name,
        "tmp/review-media",
        path(&dest),
        "--recursive",
    ]));

    assert_eq!(
        value["files"],
        serde_json::json!([
            {"path": "tmp/review-media/shot-1.png", "bytes": 4},
            {"path": "tmp/review-media/walkthrough/video.webm", "bytes": 10},
        ])
    );
    assert_eq!(value["dest"], path(&dest));
    assert_eq!(
        std::fs::read(dest.join("shot-1.png")).expect("shot"),
        b"png!"
    );
    assert_eq!(
        std::fs::read(dest.join("walkthrough/video.webm")).expect("video"),
        b"webm bytes"
    );
    let siblings: Vec<_> = std::fs::read_dir(project.root.join("out"))
        .expect("read out dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(siblings, ["media"], "no temp dir left next to dest");
}

#[test]
fn get_refuses_a_directory_without_recursive() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let dest = project.local_dir("out").join("media");

    let err = json_failure(&project.skulk(&["get", &name, "tmp/review-media", path(&dest)]));

    assert_eq!(err["code"], "validation");
    assert!(!dest.exists());
}

#[test]
fn get_list_reports_files_without_copying() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();

    let cwd = project.local_dir("cwd");

    let out = project.skulk_in(&cwd, &["get", &name, "tmp/review-media", "--list"]);

    let value = json_success(&out);
    assert_eq!(value["v"], 1);
    assert_eq!(value["dest"], serde_json::Value::Null);
    assert_eq!(
        value["files"],
        serde_json::json!([
            {"path": "tmp/review-media/shot-1.png", "bytes": 4},
            {"path": "tmp/review-media/walkthrough/video.webm", "bytes": 10},
        ])
    );
    assert!(!cwd.join("review-media").exists());
}

#[test]
fn get_refuses_when_the_files_exceed_the_cap() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let big = project.worktree(&name).join("big.bin");
    std::fs::write(&big, vec![0u8; 1_000_001]).expect("write big file");
    let dest = project.local_dir("out").join("big.bin");

    let err =
        json_failure(&project.skulk(&["get", &name, "big.bin", path(&dest), "--max-mb", "1"]));

    assert_eq!(err["code"], "validation");
    assert!(!dest.exists());
}

#[test]
fn get_refuses_paths_with_dot_dot() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let dest = project.local_dir("out").join("config.toml");

    let err =
        json_failure(&project.skulk(&["get", &name, "../../.skulk/config.toml", path(&dest)]));

    assert_eq!(err["code"], "validation");
    assert!(!dest.exists());
}

#[test]
fn get_refuses_absolute_paths() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let secret = project.root.join(".skulk/config.toml");
    let dest = project.local_dir("out").join("config.toml");

    let err = json_failure(&project.skulk(&["get", &name, path(&secret), path(&dest)]));

    assert_eq!(err["code"], "validation");
    assert!(!dest.exists());
}

#[cfg(unix)]
#[test]
fn get_refuses_a_symlink_leading_outside_the_worktree() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let outside = project.local_dir("outside");
    std::fs::write(outside.join("secret"), b"secret").expect("write secret");
    let worktree = project.worktree(&name);
    std::os::unix::fs::symlink(outside.join("secret"), worktree.join("leak")).expect("symlink");
    let dest = project.local_dir("out").join("leak");

    let err = json_failure(&project.skulk(&["get", &name, "leak", path(&dest)]));

    assert_eq!(err["code"], "validation");
    assert!(!dest.exists());
}

#[cfg(unix)]
#[test]
fn get_refuses_a_directory_holding_a_symlink_leading_outside() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let outside = project.local_dir("outside");
    std::fs::write(outside.join("secret"), b"secret").expect("write secret");
    let media = project.worktree(&name).join("tmp/review-media");
    std::os::unix::fs::symlink(&outside, media.join("escape")).expect("symlink");
    let dest = project.local_dir("out").join("media");

    let err = json_failure(&project.skulk(&[
        "get",
        &name,
        "tmp/review-media",
        path(&dest),
        "--recursive",
    ]));

    assert_eq!(err["code"], "validation");
    assert!(!dest.exists());
}

#[cfg(unix)]
#[test]
fn get_copies_a_symlink_that_stays_inside_as_a_regular_file() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let worktree = project.worktree(&name);
    std::fs::write(worktree.join("notes.txt"), b"notes").expect("write notes");
    std::os::unix::fs::symlink(
        worktree.join("notes.txt"),
        worktree.join("tmp/review-media/notes.txt"),
    )
    .expect("symlink");
    let dest = project.local_dir("out").join("media");

    let value = json_success(&project.skulk(&[
        "get",
        &name,
        "tmp/review-media",
        path(&dest),
        "--recursive",
    ]));

    assert_eq!(value["files"].as_array().map(Vec::len), Some(3));
    let copied = dest.join("notes.txt");
    assert!(!copied.is_symlink());
    assert_eq!(std::fs::read(&copied).expect("copied link"), b"notes");
}

#[cfg(unix)]
#[test]
fn get_refuses_a_missing_path_under_a_symlink_leading_outside() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();
    let outside = project.local_dir("outside");
    std::os::unix::fs::symlink(&outside, project.worktree(&name).join("docs")).expect("symlink");

    let err = json_failure(&project.skulk(&["get", &name, "docs/nope.png"]));

    assert_eq!(err["code"], "validation");
}

#[test]
fn get_emits_not_found_when_the_path_is_missing() {
    let project = Project::with_base_clone();
    let name = project.agent_with_media();

    let err = json_failure(&project.skulk(&["get", &name, "tmp/review-media/nope.png"]));

    assert_eq!(err["code"], "not_found");
    assert!(
        err["error"]
            .as_str()
            .is_some_and(|e| e.contains("nope.png")),
        "{err}"
    );
}

#[test]
fn get_emits_not_found_when_the_agent_is_missing() {
    let project = Project::with_base_clone();

    let err = json_failure(&project.skulk(&["get", &unique("ghost"), "a.png"]));

    assert_eq!(err["code"], "not_found");
}

// ── events ──────────────────────────────────────────────────────────────

/// A running `skulk --json events` whose stdout lines arrive on a channel.
/// Dropping it ends the watcher and then skulk.
struct EventStream {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl EventStream {
    fn start(project: &Project) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_skulk"))
            .args(["--json", "--config", path(&project.root), "events"])
            .current_dir(std::env::temp_dir())
            .env("SKULK_NO_UPDATE_CHECK", "1")
            .env("HOME", project.home())
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", project.root.join("tmux"))
            .env_remove("TMUX")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn skulk events");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    /// The next line whose `type` is `kind`, waiting at most `within`.
    fn next_of_type(&self, kind: &str, within: Duration) -> serde_json::Value {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {kind} line within {within:?}"));
            let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
            assert_eq!(value["v"], 1, "missing schema version: {value}");
            if value["type"] == kind {
                return value;
            }
        }
    }

    /// Kill the processes skulk started: its connection to the "remote".
    fn drop_connection(&self) {
        let out = Command::new("pkill")
            .args(["-P", &self.child.id().to_string()])
            .output()
            .expect("run pkill");
        assert!(out.status.success(), "no watcher process to kill");
    }

    /// Wait up to `within` for skulk to exit.
    fn wait_exit(&mut self, within: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll skulk") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "skulk did not exit within {within:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        let _ = Command::new("pkill")
            .args(["-P", &self.child.id().to_string()])
            .output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_marker(project: &Project, session: &str, contents: &str) {
    let dir = project.home().join(".skulk/state");
    std::fs::create_dir_all(&dir).expect("create state dir");
    std::fs::write(dir.join(session), contents).expect("write marker");
}

#[test]
fn events_reports_an_idle_state_change_within_two_seconds() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    project.stage_running_agent("busy-one");
    write_marker(&project, "skulk-busy-one", "busy");
    let events = EventStream::start(&project);
    let snapshot = events.next_of_type("snapshot", Duration::from_secs(10));
    assert_eq!(snapshot["agents"][0]["state"], "working", "{snapshot}");

    write_marker(&project, "skulk-busy-one", "idle");
    let change = events.next_of_type("idle_state", Duration::from_secs(2));

    assert_eq!(change["agent"], "busy-one", "{change}");
    assert_eq!(change["from"], "working", "{change}");
    assert_eq!(change["to"], "idle", "{change}");
}

#[test]
fn events_exits_non_zero_when_the_connection_drops() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let mut events = EventStream::start(&project);
    events.next_of_type("snapshot", Duration::from_secs(10));

    events.drop_connection();
    let status = events.wait_exit(Duration::from_secs(5));

    assert!(!status.success(), "expected a non-zero exit, got {status}");
}
