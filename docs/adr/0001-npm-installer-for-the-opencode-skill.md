# The OpenCode installer publishes as `@frantufro/skulk`

OpenCode reads skills from directories on disk, so shipping skulk's agent skill
to OpenCode users means getting a `SKILL.md` into one of them. We publish a
zero-dependency npm package whose whole job is `npx @frantufro/skulk install`:
it copies `claude-plugin/skills/` into `~/.config/opencode/skills/` (or
`./.opencode/skills/` with `--project`) and downloads the release binary when
`skulk` is absent from `PATH`.

The name is scoped because the bare one belongs to someone else. `skulk` on npm
is a Browserify bundler and livereloading server, maintained by `f0x52` and last
published in April 2025. The `@frantufro` scope was already ours, so it was
reachable the same afternoon. This differs from a name blocked by npm's
typosquat filter: an owner exists, so the bare name could become available some
day through a transfer, and the scoped name is the one we build on regardless.

## Consequences

The package's executable is named `skulk-install`, and that name is load
bearing. An executable named `skulk` would offer an installer where users expect
the agent CLI, and a global install could shadow the Homebrew or `install.sh`
copy with `PATH` order silently deciding which one runs. A distinct executable
name makes `npm i -g @frantufro/skulk` harmless: npm exposes `skulk-install`
and the real binary keeps its place. `npx @frantufro/skulk install` still works,
because npm runs a package's single declared executable whatever that
executable is called.

Four more consequences follow from the shape:

- The installer copies skill files into place. An OpenCode plugin can add its
  own directory to `config.skills.paths` at startup, which serves the skill with
  no copy at all, and published OpenCode plugins do work that way. We chose the
  one-shot installer so the skill keeps working with no plugin loaded, no entry
  in `opencode.json`, and under `opencode --pure`. Updates then happen when
  someone runs the installer again.
- `install` downloads the latest release. The npm package's own version plays no
  part in choosing it, so an old package still lands a current binary, and the
  shipped skill file can describe a different command set than the binary beside
  it. `npx @frantufro/skulk@latest install` refreshes both together.
- The download is verified. The installer fetches the published
  `skulk-<target>.tar.gz.sha256`, hashes what it received, and refuses to
  install on a mismatch, which is the same guarantee `skulk update` already
  gives (`src/commands/update.rs`). Both paths to the binary then carry equal
  weight. `install.sh` stays as it is, a documented pipe from curl into tar
  where the user has accepted that trust model up front.
- OpenCode arrives here as a **Host**, in the sense `CONTEXT.md` fixes: the
  program that loads the skill and invokes the binary. The `harness` field of
  `.skulk/config.toml` is a separate axis, and installing the skill into
  OpenCode leaves it untouched.

Claude Code keeps a single channel of its own, the plugin published through the
`frantufro/claude-plugins` marketplace, which gives its users versioned install,
update and uninstall through `/plugin`. The installer therefore takes no `--host`
flag: a directory copy into `~/.claude/skills/` would produce a second,
unmanaged copy of the same skill that `/plugin` cannot see. A request for a
second host — Cursor, Codex — reopens this decision along with the layout and
naming questions above.

Because the skill file now ships through two channels and the versions are read
by users in three places, the release workflow gates on them: a `verify` job
asserts that the tag, `Cargo.toml`, `package.json` and
`claude-plugin/.claude-plugin/plugin.json` all agree, and it runs before any
artifact is built. `plugin.json` had already drifted two releases behind the
crate when this landed.
