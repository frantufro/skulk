---
created: 2026-11-12
---

# Return io.rs to the system edge

Source: architecture review 2026-11-12, candidate 5 (Speculative). Independent of the other tasks.

Problem: io.rs (743 lines) holds command logic (cmd_logs_follow :392, run_init :464, find_new_content_start) under the io.rs coverage exclusion. LocalOps has 14 methods, several of which pass through to std::fs (remove_file, path_exists, create_dir_all, remove_dir_all). Init and Completions reach run through an unreachable!() path.

Change: move command logic into commands/ with co-located tests; narrow LocalOps to the operations upload and download use.

Wins: coverage reaches logic that was hidden in io.rs; the LocalOps interface shrinks.
