---
created: 2026-11-12
---

# Move argument interpretation from main.rs into the commands

Source: architecture review 2026-11-12, candidate 4 (Worth exploring). Independent of the other tasks.

Problem: run() (main.rs:478-615) is a 137-line match with logic in the Diff, Send, Wait, New and Replay arms. cmd_new and cmd_replay take 8-9 positional arguments. Send and Wait carry runtime Validation errors that clap already guarantees.

Change: each command takes an options struct and interprets its own flags; run() dispatches one line per arm; dead invariant checks are deleted.

Wins: flag semantics sit beside the command; the interface shrinks from 9 positional arguments to one struct; arms become testable without building a whole Cli.
