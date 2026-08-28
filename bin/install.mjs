#!/usr/bin/env node
// Installs skulk's agent skill into OpenCode's skill directories, and fetches
// the skulk binary when the host does not already have one.
//
// The skill files shipped here are the same ones the Claude Code plugin uses:
// claude-plugin/skills/ is the single source of truth, packed verbatim into
// this package. The binary download mirrors install.sh, verifies the published
// SHA-256 the way `skulk update` does, and honours SKULK_* environment hooks so
// it can be tested offline.

import { spawnSync } from "node:child_process"
import { createHash } from "node:crypto"
import { createInterface } from "node:readline"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { fileURLToPath } from "node:url"
import { createRequire } from "node:module"

const REPO = "frantufro/skulk"
const BIN_NAME = "skulk"
const PKG_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const SKILLS_SRC = path.join(PKG_ROOT, "claude-plugin", "skills")
const PKG_VERSION = createRequire(import.meta.url)("../package.json").version
const USER_AGENT = `skulk-install/${PKG_VERSION}`

const API_TIMEOUT_MS = 10_000
const DOWNLOAD_TIMEOUT_MS = 60_000

const USAGE = `skulk-install — install the skulk skill into OpenCode

usage:
  npx @frantufro/skulk install [options]

options:
  --project         install into ./.opencode/skills (default: ~/.config/opencode/skills)
  --force           overwrite an existing SKILL.md without asking
  --skip-existing   keep an existing SKILL.md and exit 0
  --no-binary       install the skill only; never download the skulk binary
  -h, --help        print this message

The skulk binary is downloaded only when \`${BIN_NAME}\` is absent from PATH.
When it is present, \`${BIN_NAME} update\` handles upgrades in place.`

function apiBase() {
  return process.env.SKULK_GITHUB_API_BASE || "https://api.github.com"
}

function downloadBase() {
  return process.env.SKULK_DOWNLOAD_BASE || "https://github.com"
}

function parseArgs(argv) {
  const flags = {
    command: null,
    project: false,
    force: false,
    skipExisting: false,
    binary: true,
    help: false,
  }
  for (const arg of argv) {
    switch (arg) {
      case "install":
        flags.command = "install"
        break
      case "--project":
        flags.project = true
        break
      case "--force":
        flags.force = true
        break
      case "--skip-existing":
        flags.skipExisting = true
        break
      case "--no-binary":
        flags.binary = false
        break
      case "-h":
      case "--help":
        flags.help = true
        break
      default:
        return { error: `unknown argument: ${arg}` }
    }
  }
  return { flags }
}

function tilde(target) {
  const home = os.homedir()
  return home && target.startsWith(home + path.sep) ? "~" + target.slice(home.length) : target
}

function report(mark, label, message) {
  process.stdout.write(`  ${mark} ${label.padEnd(7)} ${message}\n`)
}

function fail(message) {
  process.stderr.write(`  ✗ ${message}\n`)
}

// ---------------------------------------------------------------- skill files

function skillsRoot(project) {
  if (project) return path.join(process.cwd(), ".opencode", "skills")
  const xdg = process.env.XDG_CONFIG_HOME
  const base = xdg && xdg.trim() !== "" ? xdg : path.join(os.homedir(), ".config")
  return path.join(base, "opencode", "skills")
}

function packagedSkills() {
  return fs
    .readdirSync(SKILLS_SRC, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .filter((name) => fs.existsSync(path.join(SKILLS_SRC, name, "SKILL.md")))
    .sort()
}

function confirm(question) {
  return new Promise((resolve) => {
    const rl = createInterface({ input: process.stdin, output: process.stderr })
    rl.question(`  ? ${question} [y/N] `, (answer) => {
      rl.close()
      resolve(/^y(es)?$/i.test(answer.trim()))
    })
  })
}

async function installSkill(name, root, flags) {
  const source = fs.readFileSync(path.join(SKILLS_SRC, name, "SKILL.md"))
  const destDir = path.join(root, name)
  const destFile = path.join(destDir, "SKILL.md")
  const shown = tilde(destDir)

  if (fs.existsSync(destFile)) {
    if (fs.readFileSync(destFile).equals(source)) {
      report("=", "skill", `up to date  ${shown}`)
      return true
    }
    if (flags.skipExisting) {
      report("→", "skill", `kept existing  ${shown}`)
      return true
    }
    if (!flags.force) {
      if (!process.stdin.isTTY) {
        fail(`${tilde(destFile)} differs from the packaged skill`)
        process.stderr.write("    pass --force to overwrite, or --skip-existing to keep it\n")
        return false
      }
      const overwrite = await confirm(`${tilde(destFile)} differs from the packaged skill. Overwrite?`)
      if (!overwrite) {
        report("→", "skill", `kept existing  ${shown}`)
        return true
      }
    }
    fs.writeFileSync(destFile, source)
    report("↑", "skill", `updated  ${shown}`)
    return true
  }

  fs.mkdirSync(destDir, { recursive: true })
  fs.writeFileSync(destFile, source)
  report("✓", "skill", `installed  ${shown}`)
  return true
}

// --------------------------------------------------------------- skulk binary

function targetFromParts(arch, platform) {
  const osName = { linux: "unknown-linux-gnu", darwin: "apple-darwin", macos: "apple-darwin" }[platform]
  if (!osName) throw new Error(`Unsupported OS: ${platform}`)
  const normalized = { x64: "x86_64", x86_64: "x86_64", arm64: "aarch64", aarch64: "aarch64" }[arch]
  if (!normalized) throw new Error(`Unsupported architecture: ${arch}`)
  if (normalized === "x86_64" && osName === "apple-darwin") {
    throw new Error("x86_64 macOS is not supported. Use an Apple Silicon Mac or build from source.")
  }
  return { triple: `${normalized}-${osName}` }
}

function detectTarget() {
  const override = process.env.SKULK_TARGET_OVERRIDE
  if (override) {
    if (override.includes(":")) {
      const [arch, platform] = override.split(":")
      return targetFromParts(arch, platform)
    }
    return { triple: override }
  }
  return targetFromParts(process.arch, process.platform)
}

function findOnPath(name) {
  for (const dir of (process.env.PATH || "").split(path.delimiter).filter(Boolean)) {
    const candidate = path.join(dir, name)
    try {
      if (fs.statSync(candidate).isFile()) {
        fs.accessSync(candidate, fs.constants.X_OK)
        return candidate
      }
    } catch {}
  }
  return null
}

function versionOf(binary) {
  const probe = spawnSync(binary, ["--version"], { encoding: "utf8" })
  const match = /(\d+\.\d+\.\d+)/.exec(`${probe.stdout || ""}${probe.stderr || ""}`)
  return match ? match[1] : null
}

function isNewer(latest, current) {
  const parse = (value) => {
    const match = /^v?(\d+)\.(\d+)\.(\d+)/.exec(value || "")
    return match ? match.slice(1, 4).map(Number) : null
  }
  const a = parse(latest)
  const b = parse(current)
  if (!a || !b) return false
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] > b[i]
  }
  return false
}

async function latestVersion() {
  const url = `${apiBase()}/repos/${REPO}/releases/latest`
  const res = await fetch(url, {
    headers: { accept: "application/vnd.github+json", "user-agent": USER_AGENT },
    signal: AbortSignal.timeout(API_TIMEOUT_MS),
  })
  if (!res.ok) throw new Error(`GitHub API returned HTTP ${res.status}`)
  const body = await res.json()
  const tag = body && body.tag_name
  if (typeof tag !== "string") throw new Error("missing tag_name in GitHub API response")
  return tag.replace(/^v/, "")
}

function binaryDir() {
  const asRoot = typeof process.getuid === "function" && process.getuid() === 0
  return asRoot ? "/usr/local/bin" : path.join(os.homedir(), ".local", "bin")
}

// Extract the hex digest from a sha256sum-style file (`<hash>  <filename>` or
// just `<hash>`), matching parse_sha256_file in src/commands/update.rs.
function parseSha256File(content) {
  const token = (content || "").trim().split(/\s+/)[0] || ""
  return /^[0-9a-fA-F]{64}$/.test(token) ? token.toLowerCase() : null
}

async function expectedChecksum(url) {
  const res = await fetch(url, {
    redirect: "follow",
    headers: { "user-agent": USER_AGENT },
    signal: AbortSignal.timeout(API_TIMEOUT_MS),
  })
  if (!res.ok) {
    throw new Error(`missing checksum asset ${path.basename(url)}: HTTP ${res.status}`)
  }
  const digest = parseSha256File(await res.text())
  if (!digest) {
    throw new Error(`could not parse checksum file ${path.basename(url)} (expected sha256sum format)`)
  }
  return digest
}

async function downloadBinary(version, target, destDir) {
  const archive = `${BIN_NAME}-${target.triple}.tar.gz`
  const base = `${downloadBase()}/${REPO}/releases/download/v${version}`
  // Fetch the digest first: a release without one gets no further work.
  const expected = await expectedChecksum(`${base}/${archive}.sha256`)

  const res = await fetch(`${base}/${archive}`, {
    redirect: "follow",
    headers: { "user-agent": USER_AGENT },
    signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS),
  })
  if (!res.ok) throw new Error(`download failed: HTTP ${res.status} for ${base}/${archive}`)

  const work = fs.mkdtempSync(path.join(os.tmpdir(), "skulk-install-"))
  try {
    const tarball = path.join(work, archive)
    const bytes = Buffer.from(await res.arrayBuffer())
    fs.writeFileSync(tarball, bytes)

    const computed = createHash("sha256").update(bytes).digest("hex")
    if (computed !== expected) {
      throw new Error(`checksum mismatch (expected ${expected}, got ${computed})`)
    }
    report("=", "binary", `sha256 verified  ${archive}`)

    const tar = spawnSync("tar", ["-xzf", tarball, "-C", work, BIN_NAME], { encoding: "utf8" })
    if (tar.error) throw new Error(`could not run tar: ${tar.error.message}`)
    if (tar.status !== 0) throw new Error(`tar failed: ${(tar.stderr || "").trim()}`)

    fs.mkdirSync(destDir, { recursive: true })
    // Stage inside destDir so the final step is a same-filesystem rename.
    const stage = path.join(destDir, `.${BIN_NAME}.install.${process.pid}.tmp`)
    const dest = path.join(destDir, BIN_NAME)
    try {
      fs.copyFileSync(path.join(work, BIN_NAME), stage)
      fs.chmodSync(stage, 0o755)
      fs.renameSync(stage, dest)
    } catch (err) {
      fs.rmSync(stage, { force: true })
      throw err
    }
    return dest
  } finally {
    fs.rmSync(work, { recursive: true, force: true })
  }
}

function warnIfNotOnPath(dir) {
  const entries = (process.env.PATH || "").split(path.delimiter)
  if (entries.includes(dir)) return
  process.stdout.write(`\n  note: ${tilde(dir)} is not in your PATH. Add this to your shell config:\n`)
  process.stdout.write(`        export PATH="${tilde(dir)}:$PATH"\n`)
}

async function ensureBinary() {
  const existing = findOnPath(BIN_NAME)
  if (existing) {
    const current = versionOf(existing)
    report("=", "binary", `${BIN_NAME} ${current || "(unknown version)"} at ${tilde(existing)}`)
    try {
      const latest = await latestVersion()
      if (isNewer(latest, current)) {
        process.stdout.write(`    ${latest} is available — run \`${BIN_NAME} update\`\n`)
      }
    } catch {
      // A version check is a courtesy; a network failure here changes nothing.
    }
    return true
  }

  let target
  try {
    target = detectTarget()
  } catch (err) {
    fail(err.message)
    return false
  }

  try {
    const version = await latestVersion()
    report("↓", "binary", `downloading ${BIN_NAME} ${version} for ${target.triple}…`)
    const dir = binaryDir()
    const dest = await downloadBinary(version, target, dir)
    report("✓", "binary", tilde(dest))
    warnIfNotOnPath(dir)
    return true
  } catch (err) {
    fail(`could not install the ${BIN_NAME} binary: ${err.message}`)
    process.stderr.write("    install it directly instead:\n")
    process.stderr.write("      brew install frantufro/tap/skulk\n")
    process.stderr.write(`      curl -sSL https://raw.githubusercontent.com/${REPO}/main/install.sh | sh\n`)
    return false
  }
}

// --------------------------------------------------------------------- entry

async function main() {
  const { flags, error } = parseArgs(process.argv.slice(2))
  if (error) {
    process.stderr.write(`${error}\n\n${USAGE}\n`)
    return 1
  }
  if (flags.help || flags.command !== "install") {
    process.stdout.write(`${USAGE}\n`)
    return flags.help || process.argv.length <= 2 ? 0 : 1
  }

  const root = skillsRoot(flags.project)
  let ok = true
  for (const name of packagedSkills()) {
    if (!(await installSkill(name, root, flags))) ok = false
  }
  if (flags.binary && !(await ensureBinary())) ok = false
  return ok ? 0 : 1
}

main().then(
  (code) => process.exit(code),
  (err) => {
    fail(err && err.stack ? err.stack : String(err))
    process.exit(1)
  },
)
