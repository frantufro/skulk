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
        let harness = project.harness();
        project.set_harness_ignoring_enters(0);
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

    fn harness(&self) -> PathBuf {
        self.root.join("bin/harness")
    }

    /// Install a stub harness that drops the first `ignored` Enters, as Claude
    /// Code does when one arrives mid-paste, then shows the working indicator
    /// for each later one.
    ///
    /// The tty goes non-canonical first: in canonical mode the kernel caps a
    /// line (4096 bytes on Linux, 1024 on macOS) and drops the rest of a
    /// 10 KiB prompt. `min 1 time 0` makes each read return per byte, since
    /// `-icanon` alone keeps the VEOF value in the shared VMIN slot.
    fn set_harness_ignoring_enters(&self, ignored: u32) {
        let script = format!(
            "#!/bin/sh\n\
             stty -icanon min 1 time 0\n\
             echo harness-ready\n\
             n=0\n\
             while IFS= read -r _; do\n\
             \x20 n=$((n + 1))\n\
             \x20 [ \"$n\" -gt {ignored} ] && echo 'Working... (esc to interrupt)'\n\
             done\n\
             exec sleep 600\n"
        );
        std::fs::write(self.harness(), script).expect("write stub harness");
        make_executable(&self.harness());
    }

    /// Write the agent's idle marker (`idle`, `busy` or `pending`), which the
    /// harness hooks maintain on a real host.
    fn set_idle_marker(&self, name: &str, state: &str) {
        let dir = self.home().join(".skulk/state");
        std::fs::create_dir_all(&dir).expect("create state dir");
        std::fs::write(dir.join(format!("skulk-{name}")), state).expect("write idle marker");
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

    /// Wait up to ten seconds for `needle` to show in the session's pane.
    fn wait_for_pane(&self, session: &str, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = self
                .private_tmux()
                .args(["capture-pane", "-p", "-t", session])
                .output()
                .expect("run tmux");
            if String::from_utf8_lossy(&out.stdout).contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{needle} never showed in {session}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
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

// ── destroy-all ─────────────────────────────────────────────────────────

#[test]
fn destroy_all_force_emits_one_json_result_naming_every_agent() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let running = unique("a-running");
    let archived = unique("b-archived");
    project.stage_running_agent(&running);
    project.stage_archived_agent(&archived);

    let value = json_success(&project.skulk(&["destroy-all", "--force"]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agents"], serde_json::json!([running, archived]));
    assert!(!project.has_session(&format!("skulk-{running}")));
    assert!(
        !project
            .root
            .join("worktrees")
            .join(format!("skulk-{archived}"))
            .exists()
    );
}

#[test]
fn destroy_all_force_emits_json_diagnostic_when_an_agent_is_left_behind() {
    let project = Project::with_base_clone();
    let name = unique("stuck");
    // The base clone has the agent's branch checked out, so `git branch -D`
    // refuses to delete it.
    git(
        &project.base_path(),
        &["checkout", "-q", "-b", &format!("skulk-{name}")],
    );

    let err = json_failure(&project.skulk(&["destroy-all", "--force"]));

    assert_eq!(err["code"], "diagnostic");
    let message = err["error"].as_str().expect("error message");
    assert!(message.contains(&name), "must name the leftover: {message}");
}

// ── wait ────────────────────────────────────────────────────────────────

#[test]
fn wait_timeout_prints_its_payload_on_stdout_and_exits_1() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let name = unique("busy");
    project.stage_running_agent(&name);
    project.set_idle_marker(&name, "busy");

    let out = project.skulk(&["wait", &name, "--timeout", "0"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.is_empty(),
        "stderr must stay empty: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = single_json_object(&out.stdout);
    assert_eq!(value["ok"], false);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(value["reason"], "timeout");
}

#[test]
fn wait_all_reports_every_agent_and_exits_1_after_a_timeout() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    let busy = unique("a-busy");
    let idle = unique("b-idle");
    project.stage_running_agent(&busy);
    project.stage_running_agent(&idle);
    project.set_idle_marker(&busy, "busy");
    project.set_idle_marker(&idle, "idle");

    let out = project.skulk(&["wait", "--all", "--timeout", "0"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty(), "stderr must stay empty");
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON line"))
        .collect();
    let result_for = |name: &str| {
        lines
            .iter()
            .find(|v| v["agent"] == name)
            .unwrap_or_else(|| panic!("no result line for {name}: {lines:?}"))
    };
    assert_eq!(result_for(&busy)["reason"], "timeout");
    assert_eq!(result_for(&idle)["status"], "idle");
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
fn replay_emits_json_not_found_when_no_prompt_is_stored() {
    let project = Project::with_base_clone();
    let source = unique("never-run");
    let replayed = unique("replayed");

    let err = json_failure(&project.skulk(&["replay", &source, "--as", &replayed]));

    assert_eq!(err["code"], "not_found");
    let message = err["error"].as_str().expect("error message");
    assert!(message.contains(&source), "must name the agent: {message}");
    assert!(
        !project
            .root
            .join("worktrees")
            .join(format!("skulk-{replayed}"))
            .exists(),
        "no agent may be created without a prompt"
    );
}

#[test]
fn new_submits_a_long_prompt_when_the_first_enter_is_swallowed() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    project.set_harness_ignoring_enters(1);
    let name = unique("long");
    let prompt = project.root.join("prompt.txt");
    std::fs::write(&prompt, "x".repeat(10 * 1024)).expect("write prompt");

    let value = json_success(&project.skulk(&["new", &name, "--from", path(&prompt)]));

    assert_eq!(value["prompt"], "delivered");
    assert_eq!(value["warnings"], serde_json::json!([]));
}

#[test]
fn new_reports_not_submitted_when_the_prompt_stays_in_the_input_box() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    project.set_harness_ignoring_enters(1_000);
    let name = unique("stuck");
    let prompt = project.root.join("prompt.txt");
    std::fs::write(&prompt, "x".repeat(10 * 1024)).expect("write prompt");

    let value = json_success(&project.skulk(&["new", &name, "--from", path(&prompt)]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["prompt"], "not_submitted");
    let warnings = value["warnings"].as_array().expect("warnings array");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|w| w.contains("not submitted"))),
        "{warnings:?}"
    );
    assert!(project.has_session(&format!("skulk-{name}")));
    let marker = project.home().join(format!(".skulk/state/skulk-{name}"));
    assert_eq!(
        std::fs::read_to_string(marker).expect("read idle marker"),
        "idle",
        "a prompt left unsent must not leave the agent looking busy"
    );
}

#[test]
fn send_submits_a_long_prompt_when_the_first_enter_is_swallowed() {
    if !tmux_available() {
        return;
    }
    let project = Project::with_base_clone();
    project.set_harness_ignoring_enters(1);
    let name = unique("send");
    json_success(&project.skulk(&["new", &name]));
    project.wait_for_pane(&format!("skulk-{name}"), "harness-ready");
    let prompt = project.root.join("follow-up.txt");
    std::fs::write(&prompt, "y".repeat(10 * 1024)).expect("write prompt");

    let value = json_success(&project.skulk(&["send", &name, "--from", path(&prompt)]));

    assert_eq!(value["ok"], true);
    assert_eq!(value["agent"], name.as_str());
    assert_eq!(value["prompt"], "delivered");
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

// ── git-log ─────────────────────────────────────────────────────────────

impl Project {
    fn commit_in_agent(&self, name: &str, subject: &str) {
        let worktree = self.root.join("worktrees").join(format!("skulk-{name}"));
        git(
            &worktree,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "--allow-empty-message",
                "-m",
                subject,
            ],
        );
    }
}

#[test]
fn git_log_emits_empty_commits_for_a_branch_with_none() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("quiet");

    let out = project.skulk(&["git-log", "quiet"]);

    let json = json_success(&out);
    assert_eq!(json["agent"], "quiet");
    assert_eq!(json["base"], "main");
    assert_eq!(json["commits"], serde_json::json!([]));
}

#[test]
fn git_log_emits_one_commit_with_all_fields() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("one");
    project.commit_in_agent("one", "add thing");

    let json = json_success(&project.skulk(&["git-log", "one"]));

    let commits = json["commits"].as_array().expect("commits array");
    assert_eq!(commits.len(), 1);
    let c = &commits[0];
    assert_eq!(c["subject"], "add thing");
    assert_eq!(c["author"], "skulk-test");
    let sha = c["sha"].as_str().expect("sha");
    assert_eq!(sha.len(), 40);
    assert!(sha.starts_with(c["short"].as_str().expect("short")));
    for key in ["authored_at", "committed_at"] {
        let ts = c[key].as_str().expect("timestamp");
        assert!(
            ts.len() >= 20 && ts.as_bytes()[10] == b'T',
            "{key} is not ISO 8601: {ts}"
        );
    }
}

#[test]
fn git_log_lists_several_commits_newest_first_and_honours_limit() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("many");
    for subject in ["first", "second", "third"] {
        project.commit_in_agent("many", subject);
    }

    let all = json_success(&project.skulk(&["git-log", "many"]));
    let subjects: Vec<&str> = all["commits"]
        .as_array()
        .expect("commits array")
        .iter()
        .map(|c| c["subject"].as_str().expect("subject"))
        .collect();
    assert_eq!(subjects, ["third", "second", "first"]);

    let limited = json_success(&project.skulk(&["git-log", "many", "--limit", "2"]));
    assert_eq!(limited["commits"].as_array().map(Vec::len), Some(2));
    assert_eq!(limited["commits"][0]["subject"], "third");
}

#[test]
fn git_log_preserves_quotes_and_unicode_in_subjects() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("odd");
    let subject = "fix \"quoted\" 'single' \\ café 日本語 🎉 $HOME `x`";
    project.commit_in_agent("odd", subject);

    let json = json_success(&project.skulk(&["git-log", "odd"]));

    assert_eq!(json["commits"][0]["subject"], subject);
}

#[test]
fn git_log_emits_json_error_when_agent_is_missing() {
    let project = Project::with_base_clone();

    let err = json_failure(&project.skulk(&["git-log", "ghost"]));

    assert_eq!(err["code"], "not_found");
}

#[test]
fn git_log_keeps_a_subject_containing_the_field_separator() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("sep");
    let subject = "a\u{1f}b\u{1f}\u{1f}c";
    project.commit_in_agent("sep", subject);

    let json = json_success(&project.skulk(&["git-log", "sep"]));

    assert_eq!(json["commits"][0]["subject"], subject);
    assert_eq!(json["commits"][0]["author"], "skulk-test");
}

#[test]
fn git_log_reports_an_empty_subject() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("blank");
    project.commit_in_agent("blank", "");

    let json = json_success(&project.skulk(&["git-log", "blank"]));

    assert_eq!(json["commits"][0]["subject"], "");
    assert_eq!(json["commits"][0]["author"], "skulk-test");
}

#[test]
fn git_log_limit_zero_emits_no_commits() {
    let project = Project::with_base_clone();
    project.stage_archived_agent("zero");
    project.commit_in_agent("zero", "something");

    let json = json_success(&project.skulk(&["git-log", "zero", "--limit", "0"]));

    assert_eq!(json["commits"], serde_json::json!([]));
}

// ── exec ────────────────────────────────────────────────────────────────

impl Project {
    /// Run `skulk --config <project> <args>` with human output.
    fn skulk_human(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_skulk"))
            .args(["--config", path(&self.root)])
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

    /// Stage an archived agent named `name` and return its worktree path.
    fn exec_agent(&self, name: &str) -> PathBuf {
        self.stage_archived_agent(name);
        self.worktree(name)
    }
}

#[test]
fn exec_returns_stdout_stderr_and_zero_exit() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let result =
        json_success(&project.skulk(&["exec", &name, "--", "sh", "-c", "echo out; echo err >&2"]));

    assert_eq!(result["v"], 1);
    assert_eq!(result["agent"], name.as_str());
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["stdout"], "out\n");
    assert_eq!(result["stderr"], "err\n");
    assert_eq!(result["stdout_truncated"], false);
}

/// A fast command ends right after skulk forks its helper subshells, so a
/// race there loses output on some runs only; concurrent runs widen the
/// window enough to hit it.
#[test]
fn exec_keeps_the_output_of_a_fast_command_on_every_run() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let results: Vec<Output> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    (0..25)
                        .map(|_| project.skulk(&["exec", &name, "--", "echo", "out"]))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("worker thread"))
            .collect()
    });

    for out in &results {
        assert_eq!(json_success(out)["stdout"], "out\n");
    }
}

#[test]
fn exec_runs_in_worktree_with_agent_environment() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    let worktree = project.exec_agent(&name);
    std::fs::write(worktree.join(".env"), "FROM_DOTENV=dot\n").expect("write .env");
    std::fs::create_dir_all(worktree.join(".claude")).expect("mkdir .claude");
    std::fs::write(
        worktree.join(".claude/settings.local.json"),
        r#"{"env":{"FROM_SETTINGS":"it's set"}}"#,
    )
    .expect("write settings");

    let result = json_success(&project.skulk(&[
        "exec",
        &name,
        "--",
        "sh",
        "-c",
        "pwd -P; echo \"$SKULK_AGENT_NAME|$FROM_DOTENV|$FROM_SETTINGS\"",
    ]));

    assert_eq!(
        result["stdout"],
        format!("{}\n{name}|dot|it's set\n", worktree.display()).as_str()
    );
}

#[test]
fn exec_reports_nonzero_exit_in_json_not_as_failure() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let result =
        json_success(&project.skulk(&["exec", &name, "--", "sh", "-c", "echo boom >&2; exit 3"]));

    assert_eq!(result["exit_code"], 3);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["stderr"], "boom\n");
}

#[test]
fn exec_human_mode_streams_output_and_exits_with_command_code() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let out = project.skulk_human(&["exec", &name, "--", "sh", "-c", "echo hi; exit 7"]);

    assert_eq!(out.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn exec_passes_arguments_with_spaces_and_quotes_verbatim() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);
    let args = [
        "two words",
        "it's",
        "say \"hi\"",
        "$HOME",
        "`id`; rm -rf /",
        "",
    ];

    let mut cmd = vec!["exec", &name, "--", "printf", "[%s]\\n"];
    cmd.extend(args);
    let result = json_success(&project.skulk(&cmd));

    let expected: String = args
        .iter()
        .fold(String::new(), |acc, a| acc + "[" + a + "]\n");
    assert_eq!(result["stdout"], expected.as_str());
    assert_eq!(result["exit_code"], 0);
}

#[test]
fn exec_timeout_kills_command_and_its_children_and_keeps_partial_output() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    let worktree = project.exec_agent(&name);
    let start = Instant::now();

    let result = json_success(&project.skulk(&[
        "exec",
        &name,
        "--timeout",
        "1",
        "--",
        "sh",
        "-c",
        "echo before; echo warn >&2; sleep 60 & echo $! > child.pid; wait",
    ]));

    assert!(
        start.elapsed() < Duration::from_secs(20),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(result["exit_code"], 124);
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["stdout"], "before\n");
    assert_eq!(result["stderr"], "warn\n");
    let child = std::fs::read_to_string(worktree.join("child.pid")).expect("child pid");
    let alive = Command::new("kill")
        .args(["-0", child.trim()])
        .output()
        .expect("run kill")
        .status
        .success();
    assert!(
        !alive,
        "child process {} survived the timeout",
        child.trim()
    );
}

#[test]
fn exec_caps_captured_output() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let result = json_success(&project.skulk(&[
        "exec",
        &name,
        "--",
        "sh",
        "-c",
        "head -c 1100000 /dev/zero | tr '\\0' a",
    ]));

    assert_eq!(result["stdout"].as_str().map(str::len), Some(1024 * 1024));
    assert_eq!(result["stdout_truncated"], true);
}

#[test]
fn exec_emits_json_error_when_agent_is_missing() {
    let project = Project::with_base_clone();

    let err = json_failure(&project.skulk(&["exec", &unique("exec"), "--", "true"]));

    assert_eq!(err["code"], "not_found");
}

#[test]
fn exec_human_mode_reports_timeout_and_exits_124() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let out = project.skulk_human(&["exec", &name, "--timeout", "1", "--", "sleep", "60"]);

    assert_eq!(out.status.code(), Some(124));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("timed out after 1s"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn exec_reads_dotenv_as_data_without_running_it() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    let worktree = project.exec_agent(&name);
    std::fs::write(
        worktree.join(".env"),
        "A=$(touch pwned)\nB=\"quoted `touch pwned2` value\"\n",
    )
    .expect("write .env");

    let result = json_success(&project.skulk(&[
        "exec",
        &name,
        "--",
        "sh",
        "-c",
        "printf '%s|%s' \"$A\" \"$B\"",
    ]));

    assert_eq!(
        result["stdout"],
        "$(touch pwned)|quoted `touch pwned2` value"
    );
    assert!(!worktree.join("pwned").exists(), ".env was executed");
    assert!(!worktree.join("pwned2").exists(), ".env was executed");
}

fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid.trim()])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Poll until `done` holds or `secs` pass; returns whether it held.
fn eventually(secs: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

fn read_pid(file: &Path) -> String {
    assert!(
        eventually(10, || file.exists()),
        "{} never appeared",
        file.display()
    );
    std::thread::sleep(Duration::from_millis(100));
    std::fs::read_to_string(file).expect("read pid file")
}

#[test]
fn exec_kills_a_term_trapping_command_and_its_child_when_skulk_goes_away() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    let worktree = project.exec_agent(&name);
    let tmpdir = project.root.join("exec-tmp");
    std::fs::create_dir_all(&tmpdir).expect("create TMPDIR");
    let mut skulk = Command::new(env!("CARGO_BIN_EXE_skulk"))
        .args(["--json", "--config", path(&project.root)])
        .args([
            "exec",
            &name,
            "--",
            "sh",
            "-c",
            r#"trap "" TERM; sh -c 'echo $$ > child.pid; exec sleep 60' & echo $$ > cmd.pid; exec sleep 60"#,
        ])
        .env("SKULK_NO_UPDATE_CHECK", "1")
        .env("HOME", project.home())
        .env("TMPDIR", &tmpdir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn skulk");
    let cmd = read_pid(&worktree.join("cmd.pid"));
    let child = read_pid(&worktree.join("child.pid"));

    skulk.kill().expect("kill skulk");
    let _ = skulk.wait();

    assert!(
        eventually(15, || !pid_alive(&cmd)),
        "command outlived skulk"
    );
    assert!(
        eventually(15, || !pid_alive(&child)),
        "child outlived skulk"
    );
    let left_in_tmpdir = || std::fs::read_dir(&tmpdir).map_or(0, Iterator::count);
    assert!(
        eventually(15, || left_in_tmpdir() == 0),
        "temp files left in TMPDIR"
    );
}

#[test]
fn exec_human_mode_stops_a_leftover_background_child_without_hanging() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    let worktree = project.exec_agent(&name);
    let start = Instant::now();

    let out = project.skulk_human(&[
        "exec",
        &name,
        "--",
        "sh",
        "-c",
        "sleep 60 & echo $! > bg.pid; echo hi",
    ]);

    assert!(
        start.elapsed() < Duration::from_secs(20),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    let bg = std::fs::read_to_string(worktree.join("bg.pid")).expect("bg pid");
    assert!(
        !pid_alive(&bg),
        "background child {} still running",
        bg.trim()
    );
}

#[test]
fn exec_timeout_kills_a_child_that_traps_term_after_the_grace_period() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    let worktree = project.exec_agent(&name);
    let start = Instant::now();

    let result = json_success(&project.skulk(&[
        "exec",
        &name,
        "--timeout",
        "1",
        "--",
        "sh",
        "-c",
        r#"sh -c 'trap "" TERM; echo $$ > child.pid; exec sleep 60' & sleep 60"#,
    ]));

    let elapsed = start.elapsed();
    assert_eq!(result["exit_code"], 124);
    assert_eq!(result["timed_out"], true);
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(20),
        "expected TERM, a grace period, then KILL; took {elapsed:?}"
    );
    let child = std::fs::read_to_string(worktree.join("child.pid")).expect("child pid");
    let alive = Command::new("kill")
        .args(["-0", child.trim()])
        .output()
        .expect("run kill")
        .status
        .success();
    assert!(!alive, "child {} survived KILL", child.trim());
}

#[test]
fn exec_human_mode_exits_255_when_the_command_does() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let out = project.skulk_human(&["exec", &name, "--", "sh", "-c", "exit 255"]);

    assert_eq!(out.status.code(), Some(255));
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn exec_reports_a_setup_failure_apart_from_the_command_exit_code() {
    let project = Project::with_base_clone();
    let name = unique("exec");
    project.exec_agent(&name);

    let out = Command::new(env!("CARGO_BIN_EXE_skulk"))
        .args(["--config", path(&project.root)])
        .args(["exec", &name, "--", "true"])
        .env("SKULK_NO_UPDATE_CHECK", "1")
        .env("HOME", project.home())
        .env("TMPDIR", project.root.join("no-such-dir"))
        .output()
        .expect("run skulk");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Could not start the command") && stderr.contains("temp directory"),
        "stderr: {stderr}"
    );
}
