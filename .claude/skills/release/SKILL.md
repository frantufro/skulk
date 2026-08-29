---
name: release
description: Cut a new skulk release. Covers the version bump across four files, the PR, the tag, and the GitHub Actions pipeline that publishes to GitHub Releases, npm and the Homebrew tap. Use when asked to release, cut a version, or ship a new version of skulk.
allowed-tools: Bash, Read, Edit
argument-hint: [version, e.g. 0.7.2]
---

# Releasing skulk

A release is a tag push. Everything after the tag runs unattended in
`.github/workflows/release.yml`.

## 1. Pick the version

Semver against the last tag:

```bash
git tag --list 'v*' --sort=-v:refname | head -3
```

Patch for fixes and dependency bumps, minor for new commands or flags, major
for a change that breaks an existing invocation or config file.

## 2. Bump the four files that carry the version

```bash
git checkout main && git pull --ff-only
git checkout -b release/vX.Y.Z
```

- `Cargo.toml`
- `Cargo.lock` — run `cargo check` and it rewrites the `skulk` entry
- `package.json`
- `claude-plugin/.claude-plugin/plugin.json`

The `verify` job reads all four and compares them to the tag. A file left
behind stops the release before any artifact is built.

## 3. Run the local gate

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings -W clippy::pedantic
cargo test --locked
npm test
```

`--locked` catches a `Cargo.toml` bump that never reached the lockfile.
`npm test` drives `bin/install.mjs` against a fixture HTTP server and needs no
network and no `npm install`.

Clippy's pedantic set moves with each stable release. CI lints with the version
pinned in `.github/workflows/ci.yml`; if the local toolchain is older, a lint
can pass here and fail there. `rustup update stable` closes the gap.

## 4. Open the PR

```bash
git commit -m "release: bump to X.Y.Z"
git push -u origin release/vX.Y.Z
gh pr create --title "release: bump to X.Y.Z" --body-file <file>
gh pr checks <PR> --watch --fail-fast
```

The body should say what the release carries — the commits already on `main`
since the last tag, in prose. `git log --oneline vX.Y.Z-1..main` is the source.

## 5. Merge

This repository allows rebase merges only. Squash and merge commits are both
disabled, and `--squash` fails with
`GraphQL: Squash merges are not allowed on this repository`.

```bash
gh pr merge <PR> --rebase --delete-branch
```

A single-commit release branch lands verbatim, so the tag can point straight
at it.

## 6. Tag

```bash
git checkout main && git pull --ff-only
git log --oneline -1          # confirm this is the release commit
git tag vX.Y.Z
git push origin vX.Y.Z
```

Pushing the tag publishes to three places at once and npm refuses to
republish a version. Get explicit confirmation from the user before this
command runs. In sandboxed sessions the classifier may block the tag push;
hand it to the user prefixed with `!` so the output lands in the conversation.

## 7. Watch the pipeline

```bash
gh run list --workflow release.yml --limit 1
gh run watch <run-id> --exit-status
```

## What the workflow does

| Job | Depends on | Result |
| --- | --- | --- |
| `verify` | — | Four versions agree with the tag, then `cargo test --locked` and `npm test` |
| `build` | `verify` | `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu` natively, `aarch64-unknown-linux-gnu` under `cross`; each tarball gets a `.sha256` sidecar |
| `release` | `build` | GitHub Release carrying the tarballs and the sidecars |
| `publish-npm` | `release` | `npm publish --access public` for `@frantufro/skulk` |
| `publish-homebrew-formula` | `release` | Regenerates `Formula/skulk.rb` in `frantufro/homebrew-tap` |

The `.sha256` files are load-bearing. `skulk update` (`src/commands/update.rs`)
and the npm installer (`bin/install.mjs`) both fetch the sidecar and refuse the
download when the digest disagrees, so a packaging change has to keep emitting
them in `sha256sum` format.

The npm publish runs on OIDC trusted publishing. The job holds `id-token:
write` and the repository stores no npm credential. The trust configuration
lives at <https://www.npmjs.com/package/@frantufro/skulk/access> and names
organization `frantufro`, repository `skulk`, workflow `release.yml`, with the
environment field empty. Registering it has to happen on that web page; the
`npm trust` CLI on npm 11.x sends a payload the registry rejects with an empty
`400`.

The Homebrew job reads `secrets.HOMEBREW_TAP_TOKEN` to push to the tap
repository.

## When something fails

**`verify` fails on a version mismatch.** Nothing has been published. Fix the
file on `main` through a normal PR, then move the tag:

```bash
git tag -d vX.Y.Z
git push origin :refs/tags/vX.Y.Z
```

and tag again once the fix has landed.

**A job after `publish-npm` fails.** The npm version is spent. That release
number can never be reused, so the recovery is a new patch version carrying
the fix.

**The published package looks missing right after a successful publish.**
The packument lags the registry's write path by minutes while
`npm access list packages` already reports the package. Poll
`https://registry.npmjs.org/@frantufro/skulk` until it returns 200 before
concluding anything went wrong.

## Afterwards

Verify what users will actually get:

```bash
npm view @frantufro/skulk version
gh release view vX.Y.Z
```
