---
status: READY
---

Add `skulk diff <name>` — show `git diff <default_branch>...{session_prefix}<name>` on the remote.

Lowest-friction way to review an agent's changes without attaching. Single SSH call wrapping `git diff`. Probably under 100 lines including tests.
