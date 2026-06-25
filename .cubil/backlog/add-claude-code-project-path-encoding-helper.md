---
created: 2026-06-25
---

# Add Claude Code project path encoding helper

## Context

Claude Code stores conversation sessions at `~/.claude/projects/<encoded-path>/` on each machine. The "encoded path" is **not** a cryptographic hash — it is the absolute filesystem path with every `/` replaced by `-`.

Discovery evidence: inspecting `~/.claude/projects/` on the development machine shows:
- Project at `/Users/alice/Documents/skulk` → directory `~/.claude/projects/-Users-alice-Documents-skulk/`
- Project at `/Users/alice/Documents/skulk-tui` → directory `~/.claude/projects/-Users-alice-Documents-skulk-tui/`

The algorithm is simply: `abs_path.replace('/', "-")`.

This helper is needed by two upcoming commands:
- **`skulk upload`**: finds the local JSONL files (encode local project dir), copies them to the remote encoded path
- **`skulk download`**: finds remote JSONL files (encode remote worktree path), copies them to the local encoded path

## Files to modify

### `src/util.rs`

Add two new functions after the existing helpers.

**Function 1 — pure path encoder:**
```rust
/// Encode an absolute filesystem path into the directory name Claude Code uses
/// under `~/.claude/projects/`.
///
/// Claude Code stores project sessions at `~/.claude/projects/<encoded>/` where
/// the encoded name is the absolute path with every `/` replaced by `-`.
///
/// Example: `/home/alice/projects/skulk` → `-home-alice-projects-skulk`
pub(crate) fn claude_project_dir_name(abs_path: &str) -> String {
    abs_path.replace('/', "-")
}
```

**Function 2 — remote SSH command builder:**
```rust
/// Build a shell command that outputs the Claude Code project directory name
/// for a remote path (which may start with `~`).
///
/// The output is suitable for appending to `~/.claude/projects/` to get the
/// session storage directory on the remote. Uses `cd <path> && pwd | tr '/' '-'`:
/// `cd` expands the tilde, `pwd` gives the canonical absolute path, `tr`
/// applies the encoding.
///
/// Requires the remote path to already exist (because `cd` fails on a
/// nonexistent directory). Call this after the remote worktree has been created.
pub(crate) fn remote_claude_project_dir_command(remote_path: &str) -> String {
    format!("cd {remote_path} && pwd | tr '/' '-'")
}
```

**Add tests** in the `#[cfg(test)]` block:

```rust
// ── claude_project_dir_name tests ──────────────────────────────────────

#[test]
fn claude_project_dir_name_simple() {
    assert_eq!(
        claude_project_dir_name("/home/alice/skulk"),
        "-home-alice-skulk"
    );
}

#[test]
fn claude_project_dir_name_nested_with_hyphens() {
    // Hyphens in path components remain as hyphens (not ambiguous for encoding)
    assert_eq!(
        claude_project_dir_name("/home/alice/worktrees/skulk-feat/add-feature"),
        "-home-alice-worktrees-skulk-feat-add-feature"
    );
}

#[test]
fn claude_project_dir_name_macOS_style() {
    assert_eq!(
        claude_project_dir_name("/Users/alice/Documents/skulk"),
        "-Users-alice-Documents-skulk"
    );
}

#[test]
fn claude_project_dir_name_root_slash() {
    assert_eq!(claude_project_dir_name("/"), "-");
}

// ── remote_claude_project_dir_command tests ─────────────────────────────

#[test]
fn remote_claude_project_dir_command_uses_cd_and_tr() {
    let cmd = remote_claude_project_dir_command("~/worktrees/skulk-foo");
    assert!(cmd.contains("cd ~/worktrees/skulk-foo"));
    assert!(cmd.contains("pwd"));
    assert!(cmd.contains("tr '/' '-'"));
}

#[test]
fn remote_claude_project_dir_command_embeds_path() {
    let path = "~/my-project-worktrees/skulk-feat/thing";
    let cmd = remote_claude_project_dir_command(path);
    assert!(cmd.contains(path));
}
```

## Verification

```
cargo fmt
cargo clippy -- -D warnings -W clippy::pedantic
cargo test util::tests
```

All new tests must pass.
