---
status: DONE
implemented: fde7c66
---

Document the `--remote-control` idle-death limitation in the README.

After `remote-control-flag` lands, `--remote-control` is opt-in. When enabled, sessions die after ~20 min of inactivity ([anthropics/claude-code#32982](https://github.com/anthropics/claude-code/issues/32982)).

**Deliverables**:
- README note next to the `--remote-control` flag explaining the limitation and linking the upstream issue
- Optional: keepalive-loop helper as part of `init.sh` examples (sends a no-op activity every 15 min when in remote-control mode)

**No code workaround required** unless the keepalive helper is wanted — the limitation is acceptable for the opt-in mobile-app use case.

**Depends on**: `remote-control-flag`.
