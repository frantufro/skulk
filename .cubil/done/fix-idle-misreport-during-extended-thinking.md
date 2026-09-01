---
created: 2026-05-14
---

# Fix idle misreport during Claude extended thinking

`skulk status` and `skulk list` report agents as **idle** while Claude is mid-turn in an extended-thinking phase ("Computing… 3m · almost done thinking"). Discovered while polling an agent that was actively working on a multi-minute reasoning step.

## Root cause

`resolve_agent_state` in `src/inventory.rs:79-91` classifies a session as `Idle` when `state_mtime >= activity`:
- `state_mtime` is the mtime of the Claude Stop-hook marker (written when Claude finishes a turn).
- `activity` is tmux's `session_activity` field — bumped when pane output changes.

During extended thinking, Claude Code rewrites the "Computing… Ns" line in place via ANSI cursor moves. tmux's `session_activity` apparently doesn't tick for this in-place redraw (or ticks too rarely), so `activity` stays frozen at the value from when the previous turn ended — which is also when the Stop hook fired. Result: `state_mtime >= activity` evaluates true and the agent looks idle.

## Symptoms

- Polling loops keep firing because the agent appears idle but never reports progress.
- Subagents that summarize "what is the agent doing" misread the state and report "stalled / finishing turns prematurely" when in fact Claude is just thinking.

## Fix options

1. **Working-marker hook**: add a `UserPromptSubmit` (or equivalent) hook that writes a `.working` marker; treat "working marker newer than Stop marker" as still-busy. Stop hook clears the working marker. This is robust to any quiet-thinking gap.
2. **Pane content inspection**: shell out to `tmux capture-pane -p` and look for known thinking indicators ("Computing…", "esc to interrupt", spinner glyphs). Cheaper than option 1 but harness-coupled and brittle to UI changes.
3. **Activity grace period**: only classify as idle when `state_mtime` is significantly newer than `activity` (e.g. >5s). Doesn't fully solve the problem — long thinking phases can still mis-trigger.

Option 1 is the cleanest. Mirrors how the Stop hook is already wired in.

## Touches

- `src/inventory.rs` — state resolution logic.
- Hooks documentation / `skulk init` — install the new working-marker hook.
- `src/ssh.rs` / `src/io.rs` — possibly read an extra marker file in the single-roundtrip state gather.
- Tests in `src/inventory.rs` — add cases for "working marker present" overrides "stop marker recent".
