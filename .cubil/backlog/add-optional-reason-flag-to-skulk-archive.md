---
created: 2026-06-25
---

# Add optional --reason flag to skulk archive

## Context

`skulk archive` kills an agent's tmux session while preserving the worktree and branch. We want to add an optional `--reason <TEXT>` flag so users can annotate why they archived an agent (e.g. `"PR merged"`, `"switched approach"`). The reason is stored in a sidecar file on the remote at `~/.skulk/archive/<session-name>.txt`.

This is a general-purpose feature. It will also be called programmatically by the upcoming `skulk download` command, which auto-archives with the reason `"downloaded to <hostname>"`.

**Key invariant:** the reason write is non-fatal. If writing the sidecar file fails (permissions, disk space), `cmd_archive` still returns `Ok` and prints a warning to stderr.

## Files to modify

### 1. `src/main.rs`

Find the `Archive` variant in the `Commands` enum (around lines 239-247):
```rust
Archive {
    /// Agent name to archive
    name: String,
},
```

Add an optional `reason` field:
```rust
Archive {
    /// Agent name to archive
    name: String,
    /// Optional annotation stored on the remote as ~/.skulk/archive/<session>.txt
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
},
```

Find the dispatch for `Commands::Archive` in `run()` (around line 511):
```rust
Commands::Archive { name } => interact::cmd_archive(ssh, &name, cfg),
```

Update to pass the reason:
```rust
Commands::Archive { name, reason } => interact::cmd_archive(ssh, &name, reason.as_deref(), cfg),
```

Also find the test(s) that construct `Commands::Archive { name }` (around line 1043 in the `#[cfg(test)]` section) and add `reason: None` to the struct literal.

### 2. `src/commands/interact.rs`

**Add a command builder function** following the pattern of other builder functions in this file:

```rust
/// Build the SSH command to write an archive reason sidecar file.
///
/// Stored at `~/.skulk/archive/<session_name>.txt` so callers can later
/// inspect why an agent was archived. `reason` must already be
/// `shell_escape`d by the caller.
pub(crate) fn archive_reason_command(session_name: &str, reason: &str) -> String {
    format!(
        "mkdir -p ~/.skulk/archive && printf '%s' '{}' > ~/.skulk/archive/{session_name}.txt",
        shell_escape(reason)
    )
}
```

Note: use `printf '%s'` rather than `echo` to avoid interpreting escape sequences in the reason text.

**Update `cmd_archive` signature** (currently at lines 240-246):
```rust
pub(crate) fn cmd_archive(ssh: &impl Ssh, name: &str, reason: Option<&str>, cfg: &Config) -> Result<(), SkulkError>
```

Inside `cmd_archive`, after the existing tmux kill call succeeds, write the sidecar if reason was provided:
```rust
if let Some(text) = reason {
    let session_name = AgentRef::new(name, cfg).session_name();
    if let Err(e) = ssh.run(&archive_reason_command(&session_name, text)) {
        eprintln!("Warning: failed to write archive reason for '{name}': {e}");
    }
}
```

### 3. Tests in `src/commands/interact.rs`

**Update existing calls to `cmd_archive`** to pass `None` for reason — grep the test section for `cmd_archive` and add the parameter.

**Add new tests:**

```rust
#[test]
fn archive_with_reason_writes_sidecar() {
    // Two SSH calls: kill session, then write reason sidecar
    let ssh = MockSsh::new(vec![ssh_ok(), ssh_ok()]);
    let cfg = test_config();
    cmd_archive(&ssh, "my-agent", Some("PR merged"), &cfg).unwrap();
    let calls = ssh.calls();
    assert_eq!(calls.len(), 2);
    // Second call writes the sidecar
    assert!(calls[1].contains("skulk-my-agent.txt"));
    assert!(calls[1].contains("PR merged"));
    assert!(calls[1].contains("~/.skulk/archive"));
}

#[test]
fn archive_without_reason_makes_one_ssh_call() {
    let ssh = MockSsh::new(vec![ssh_ok()]);
    let cfg = test_config();
    cmd_archive(&ssh, "my-agent", None, &cfg).unwrap();
    assert_eq!(ssh.calls().len(), 1);
}

#[test]
fn archive_reason_failure_is_nonfatal() {
    // First call (kill) succeeds; second call (reason write) fails
    let ssh = MockSsh::new(vec![ssh_ok(), ssh_err("disk full")]);
    let cfg = test_config();
    // Must return Ok even though the sidecar write failed
    let result = cmd_archive(&ssh, "my-agent", Some("done"), &cfg);
    assert!(result.is_ok());
}
```

**Add tests for `archive_reason_command` builder:**

```rust
#[test]
fn archive_reason_command_contains_session_name() {
    let cmd = archive_reason_command("skulk-my-agent", "PR merged");
    assert!(cmd.contains("skulk-my-agent.txt"));
}

#[test]
fn archive_reason_command_contains_reason_text() {
    let cmd = archive_reason_command("skulk-my-agent", "PR merged");
    assert!(cmd.contains("PR merged"));
}

#[test]
fn archive_reason_command_escapes_single_quotes() {
    // Reason text with single quotes must be shell-escaped
    let cmd = archive_reason_command("skulk-foo", "it's done");
    // shell_escape turns ' into '\''
    assert!(cmd.contains("it'\\''s done"));
}
```

## Verification

```
cargo fmt
cargo clippy -- -D warnings -W clippy::pedantic
cargo test
```

All interact tests must pass. Confirm `cmd_archive` is called with the new signature everywhere by running `cargo check` — it will show type errors at any call site that's missing the `reason` parameter.
