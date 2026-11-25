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
        Command::new(env!("CARGO_BIN_EXE_skulk"))
            .args(["--json", "--config", path(&self.root)])
            .args(args)
            .current_dir(std::env::temp_dir())
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
