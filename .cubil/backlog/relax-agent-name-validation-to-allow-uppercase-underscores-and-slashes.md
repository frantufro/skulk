---
created: 2026-06-25
---

# Relax agent name validation to allow uppercase, underscores, and slashes

## Context

skulk currently restricts agent names to `[a-z0-9-]`, max 30 characters (enforced in `validate_name` in `src/util.rs` lines 56-85). This was an arbitrary convention, not a hard technical constraint. We are relaxing it so that real-world git branch names can be used directly as agent names — for example `feat/add-upload`, `MyFeature`, `fix_bug_123`.

The new charset is the intersection of what tmux session names and git branch names safely support: **`[a-zA-Z0-9/_-]`**, **max 100 characters**.

**Why these specific rules:**
- Uppercase letters allowed: valid in both tmux and git
- Underscore `_` allowed: valid in both tmux and git
- Forward slash `/` allowed: valid in tmux session names; used for git branch namespacing (`feat/`, `fix/`)
- Dot `.` NOT allowed: tmux uses `.` as a window separator in target specs (`session:window.pane`), making dots in session names ambiguous
- Colon `:` NOT allowed: tmux uses `:` as a session separator in target specs
- Space, semicolons, shell metacharacters NOT allowed: would break SSH command construction
- Max 100 chars: practical limit matching real branch name lengths; 30 was too restrictive
- Cannot start with `-`: would be parsed as a CLI flag
- Cannot start with `/`: would look like an absolute path
- Cannot end with `/`: would be an empty path component
- Cannot contain `//`: git disallows consecutive slashes in branch names

The old "no consecutive hyphens" and "no trailing hyphen" rules are dropped — they were arbitrary and don't correspond to any technical constraint.

## Files to modify

### 1. `src/util.rs`

This is the primary change. Modify the `validate_name` function (lines 56-85):

**New rules to implement (replace current body):**
```rust
pub(crate) fn validate_name(name: &str) -> Result<(), SkulkError> {
    if name.is_empty() {
        return Err(SkulkError::Validation("Agent name cannot be empty.".into()));
    }
    if name.len() > 100 {
        return Err(SkulkError::Validation(
            "Agent name must be 100 characters or fewer.".into(),
        ));
    }
    if name.starts_with('-') {
        return Err(SkulkError::Validation(
            "Agent name cannot start with a hyphen.".into(),
        ));
    }
    if name.starts_with('/') {
        return Err(SkulkError::Validation(
            "Agent name cannot start with a slash.".into(),
        ));
    }
    if name.ends_with('/') {
        return Err(SkulkError::Validation(
            "Agent name cannot end with a slash.".into(),
        ));
    }
    if name.contains("//") {
        return Err(SkulkError::Validation(
            "Agent name cannot contain consecutive slashes.".into(),
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/') {
            return Err(SkulkError::Validation(format!(
                "Invalid character '{c}' in agent name. Only letters, digits, hyphens, underscores, and slashes allowed."
            )));
        }
    }
    Ok(())
}
```

Update the doc comment above `validate_name` to: `/// Validate an agent name: [a-zA-Z0-9/_-], 1-100 chars.`

**Update the existing tests (lines 127-214):**

Tests that must change behavior (they currently expect errors but the chars are now allowed):
- `validate_name_uppercase` (`"My-Task"`) → change to `assert!(validate_name("My-Task").is_ok())`
- `validate_name_underscore` (`"my_task"`) → change to `assert!(validate_name("my_task").is_ok())`

Tests that must be updated for the new limits:
- `validate_name_valid_max_length`: change to use a 100-char string: `"a".repeat(100)`; assert it's `Ok`
- `validate_name_too_long`: change to use a 101-char string: `"a".repeat(101)`; assert error contains "100 characters"

Tests to delete (these rules no longer exist):
- `validate_name_consecutive_hyphens` (the `"double--hyphen"` test) — delete entirely
- `validate_name_trailing_hyphen` (the `"trailing-"` test) — delete entirely

Tests to add:
```rust
#[test]
fn validate_name_valid_slash() {
    assert!(validate_name("feat/add-feature").is_ok());
}

#[test]
fn validate_name_valid_namespaced_slash() {
    assert!(validate_name("feat/fix/deep").is_ok());
}

#[test]
fn validate_name_leading_slash_rejected() {
    let result = validate_name("/absolute");
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("start with a slash"));
}

#[test]
fn validate_name_trailing_slash_rejected() {
    let result = validate_name("feat/");
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("end with a slash"));
}

#[test]
fn validate_name_consecutive_slashes_rejected() {
    let result = validate_name("feat//thing");
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("consecutive slashes"));
}

#[test]
fn validate_name_valid_100_chars() {
    let name = "a".repeat(100);
    assert!(validate_name(&name).is_ok());
}

#[test]
fn validate_name_101_chars_rejected() {
    let name = "a".repeat(101);
    let result = validate_name(&name);
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("100 characters"));
}
```

### 2. `README.md`

Line 354 currently reads:
> Names must be lowercase letters, digits, and hyphens. 1-30 characters. No leading, trailing, or consecutive hyphens.

Update to:
> Names may contain letters (upper or lowercase), digits, hyphens, underscores, and forward slashes. 1-100 characters. Cannot start with `-` or `/`, end with `/`, or contain `//`.

### 3. `src/agent_ref.rs`

No functional changes needed. The existing tests use `"my-task"` which remains valid. No updates required.

## Verification

After implementing, run:
```
cargo fmt
cargo clippy -- -D warnings -W clippy::pedantic
cargo test
```

All tests must pass. Pay special attention to the `util::tests` module to ensure all updated and new tests pass.
