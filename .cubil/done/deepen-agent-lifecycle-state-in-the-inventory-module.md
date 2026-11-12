---
created: 2026-11-12
---

# Deepen Agent lifecycle state in the inventory module

Source: architecture review 2026-11-12, candidate 1 (Strong).

Problem: running, archived, orphan and absent (see CONTEXT.md) are re-derived in about six commands (restart.rs:40, destroy.rs:103/165, replay.rs:127, new.rs:525, wait.rs:133, upload.rs:81, download.rs:69, gc.rs:149), each with its own error text. list.rs:50-118 and status.rs:~101-146 carry private copies of the __WORKTREES_START__ parser that inventory.rs already has.

Change: the inventory module owns the probe and the lifecycle classification per Agent. Commands match on the state. list and status drop their private parsers.

Wins: locality (a lifecycle rule changes in one place), leverage (one interface, eight call sites), two duplicate parsers deleted.

Report: architecture-review-20261112.html (session scratchpad).
