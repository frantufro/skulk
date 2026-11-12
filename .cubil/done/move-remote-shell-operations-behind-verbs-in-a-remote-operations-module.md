---
created: 2026-11-12
---

# Move remote shell operations behind verbs in a remote operations module

Source: architecture review 2026-11-12, candidate 2 (Strong). Builds on the lifecycle task.

Problem: Ssh::run takes opaque shell and every command builds its own tmux and git strings (new.rs:198/311/403/494-600, destroy.rs:13-30, gc.rs:211, interact.rs, wait.rs:43). new.rs and restart.rs:55-70 each run 'create tmux, kill session on failure' with differing detail. Tests assert on string fragments (restart.rs:371, gc.rs:509) and about 1300 lines of new.rs tests check command text.

Change: a module exposing verbs (start session, kill session, add/remove worktree, probe) that owns the shell strings and the partial-failure cleanup. new, restart, destroy, gc and wait call it.

Wins: rollback logic exists once; tests assert behaviour at the verb level; string-fragment tests shrink.
