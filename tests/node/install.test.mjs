// Integration tests for bin/install.mjs.
//
// A local HTTP server stands in for the GitHub API and the release download,
// reached through the same SKULK_GITHUB_API_BASE / SKULK_DOWNLOAD_BASE hooks
// that `skulk update` uses, so the whole suite runs offline.

import assert from "node:assert/strict"
import fs from "node:fs"
import http from "node:http"
import os from "node:os"
import path from "node:path"
import { createHash } from "node:crypto"
import { after, before, test } from "node:test"
import { spawn, spawnSync } from "node:child_process"
import { fileURLToPath } from "node:url"

const HERE = path.dirname(fileURLToPath(import.meta.url))
const REPO_ROOT = path.resolve(HERE, "..", "..")
const INSTALLER = path.join(REPO_ROOT, "bin", "install.mjs")
const SKILL_NAME = "skulk-agent-management"
const SOURCE_SKILL = path.join(REPO_ROOT, "claude-plugin", "skills", SKILL_NAME, "SKILL.md")
const LATEST = "9.9.9"
const TARGET = "aarch64-apple-darwin"
// A PATH without the developer's own skulk, still able to reach tar.
const BARE_PATH = "/usr/bin:/bin"

let server
let base
let tarball
let digest
// Set by the checksum tests to make the fixture serve a bad or absent sidecar.
let checksum = "ok"
const requested = []

function tmpdir(label) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `skulk-test-${label}-`))
}

function buildTarball() {
  const work = tmpdir("tarball")
  const fake = path.join(work, "skulk")
  fs.writeFileSync(fake, "#!/bin/sh\necho 'skulk 9.9.9'\n")
  fs.chmodSync(fake, 0o755)
  const archive = path.join(work, `skulk-${TARGET}.tar.gz`)
  const result = spawnSync("tar", ["-czf", archive, "-C", work, "skulk"], { encoding: "utf8" })
  assert.equal(result.status, 0, `fixture tar failed: ${result.stderr}`)
  return fs.readFileSync(archive)
}

before(async () => {
  tarball = buildTarball()
  digest = createHash("sha256").update(tarball).digest("hex")
  const archivePath = `/frantufro/skulk/releases/download/v${LATEST}/skulk-${TARGET}.tar.gz`
  server = http.createServer((req, res) => {
    requested.push(req.url)
    if (req.url === "/repos/frantufro/skulk/releases/latest") {
      res.writeHead(200, { "content-type": "application/json" })
      res.end(JSON.stringify({ tag_name: `v${LATEST}` }))
      return
    }
    if (req.url === `${archivePath}.sha256`) {
      if (checksum === "missing") {
        res.writeHead(404)
        res.end("not found")
        return
      }
      const body = {
        ok: `${digest}  skulk-${TARGET}.tar.gz\n`,
        mismatch: `${"0".repeat(64)}  skulk-${TARGET}.tar.gz\n`,
        garbage: "not a checksum at all\n",
      }[checksum]
      res.writeHead(200, { "content-type": "text/plain" })
      res.end(body)
      return
    }
    if (req.url === archivePath) {
      res.writeHead(200, { "content-type": "application/gzip" })
      res.end(tarball)
      return
    }
    res.writeHead(404)
    res.end("not found")
  })
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve))
  base = `http://127.0.0.1:${server.address().port}`
})

after(() => server.close())

// Asynchronous on purpose: the fixture server shares this process's event
// loop, so a blocking spawnSync would deadlock every request the installer makes.
function run(args, { home, cwd, path: pathEnv = BARE_PATH, stdin = "ignore", target = TARGET, env = {} } = {}) {
  const child = spawn(process.execPath, [INSTALLER, ...args], {
    cwd: cwd || home,
    stdio: [stdin, "pipe", "pipe"],
    env: {
      HOME: home,
      PATH: pathEnv,
      TMPDIR: process.env.TMPDIR || "/tmp",
      SKULK_GITHUB_API_BASE: base,
      SKULK_DOWNLOAD_BASE: base,
      SKULK_TARGET_OVERRIDE: target,
      ...env,
    },
  })
  let stdout = ""
  let stderr = ""
  child.stdout.setEncoding("utf8").on("data", (chunk) => (stdout += chunk))
  child.stderr.setEncoding("utf8").on("data", (chunk) => (stderr += chunk))
  return new Promise((resolve, reject) => {
    child.on("error", reject)
    child.on("close", (status) => resolve({ status, stdout, stderr }))
  })
}

function globalSkill(home) {
  return path.join(home, ".config", "opencode", "skills", SKILL_NAME, "SKILL.md")
}

test("fresh install writes a byte-identical SKILL.md", async () => {
  const home = tmpdir("fresh")
  const result = await run(["install", "--no-binary"], { home })
  assert.equal(result.status, 0, result.stderr)
  assert.match(result.stdout, /installed/)
  assert.deepEqual(fs.readFileSync(globalSkill(home)), fs.readFileSync(SOURCE_SKILL))
})

test("XDG_CONFIG_HOME is honoured for the global scope", async () => {
  const home = tmpdir("xdg")
  const xdg = path.join(home, "xdg-config")
  const result = await run(["install", "--no-binary"], { home, env: { XDG_CONFIG_HOME: xdg } })
  assert.equal(result.status, 0, result.stderr)
  assert.ok(fs.existsSync(path.join(xdg, "opencode", "skills", SKILL_NAME, "SKILL.md")))
})

test("an identical re-run reports up to date and exits 0", async () => {
  const home = tmpdir("idempotent")
  await run(["install", "--no-binary"], { home })
  const again = await run(["install", "--no-binary"], { home })
  assert.equal(again.status, 0, again.stderr)
  assert.match(again.stdout, /up to date/)
})

test("a differing file with no TTY exits 1 and names both flags", async () => {
  const home = tmpdir("blocked")
  await run(["install", "--no-binary"], { home })
  fs.appendFileSync(globalSkill(home), "\nlocal edit\n")
  const result = await run(["install", "--no-binary"], { home })
  assert.equal(result.status, 1)
  assert.match(result.stderr, /differs from the packaged skill/)
  assert.match(result.stderr, /--force/)
  assert.match(result.stderr, /--skip-existing/)
  assert.match(fs.readFileSync(globalSkill(home), "utf8"), /local edit/)
})

test("--force overwrites and --skip-existing preserves", async () => {
  const home = tmpdir("flags")
  await run(["install", "--no-binary"], { home })
  fs.appendFileSync(globalSkill(home), "\nlocal edit\n")

  const skipped = await run(["install", "--no-binary", "--skip-existing"], { home })
  assert.equal(skipped.status, 0, skipped.stderr)
  assert.match(skipped.stdout, /kept existing/)
  assert.match(fs.readFileSync(globalSkill(home), "utf8"), /local edit/)

  const forced = await run(["install", "--no-binary", "--force"], { home })
  assert.equal(forced.status, 0, forced.stderr)
  assert.match(forced.stdout, /updated/)
  assert.deepEqual(fs.readFileSync(globalSkill(home)), fs.readFileSync(SOURCE_SKILL))
})

test("--project installs into .opencode/skills under the cwd", async () => {
  const home = tmpdir("project-home")
  const project = tmpdir("project")
  const result = await run(["install", "--no-binary", "--project"], { home, cwd: project })
  assert.equal(result.status, 0, result.stderr)
  assert.ok(fs.existsSync(path.join(project, ".opencode", "skills", SKILL_NAME, "SKILL.md")))
  assert.equal(fs.existsSync(globalSkill(home)), false)
})

test("a missing binary is downloaded into ~/.local/bin and runs", async () => {
  const home = tmpdir("download")
  checksum = "ok"
  const result = await run(["install"], { home })
  assert.equal(result.status, 0, result.stderr)
  const dest = path.join(home, ".local", "bin", "skulk")
  assert.ok(fs.existsSync(dest), result.stdout)
  assert.equal(fs.statSync(dest).mode & 0o777, 0o755)
  assert.match(result.stdout, new RegExp(`downloading skulk ${LATEST}`))
  assert.match(result.stdout, /sha256 verified/)
  assert.match(spawnSync(dest, ["--version"], { encoding: "utf8" }).stdout, /9\.9\.9/)
  assert.match(result.stdout, /not in your PATH/)
  // Nothing is left behind in the destination directory.
  assert.deepEqual(fs.readdirSync(path.join(home, ".local", "bin")), ["skulk"])
})

test("a checksum mismatch refuses the download and installs nothing", async () => {
  const home = tmpdir("mismatch")
  checksum = "mismatch"
  try {
    const result = await run(["install"], { home })
    assert.equal(result.status, 1)
    assert.match(result.stderr, /checksum mismatch/)
    assert.match(result.stderr, new RegExp(`expected ${"0".repeat(64)}`))
    assert.equal(fs.existsSync(path.join(home, ".local", "bin", "skulk")), false)
  } finally {
    checksum = "ok"
  }
})

test("a release with no checksum asset is refused", async () => {
  const home = tmpdir("no-checksum")
  checksum = "missing"
  requested.length = 0
  try {
    const result = await run(["install"], { home })
    assert.equal(result.status, 1)
    assert.match(result.stderr, /missing checksum asset skulk-.*\.tar\.gz\.sha256/)
    assert.equal(fs.existsSync(path.join(home, ".local", "bin", "skulk")), false)
    // The archive itself is never fetched once the digest is unavailable.
    assert.equal(
      requested.includes(`/frantufro/skulk/releases/download/v${LATEST}/skulk-${TARGET}.tar.gz`),
      false,
    )
  } finally {
    checksum = "ok"
  }
})

test("an unparseable checksum file is refused", async () => {
  const home = tmpdir("bad-checksum")
  checksum = "garbage"
  try {
    const result = await run(["install"], { home })
    assert.equal(result.status, 1)
    assert.match(result.stderr, /could not parse checksum file/)
    assert.equal(fs.existsSync(path.join(home, ".local", "bin", "skulk")), false)
  } finally {
    checksum = "ok"
  }
})

test("an existing binary is left alone and an upgrade is suggested", async () => {
  const home = tmpdir("existing")
  const binDir = path.join(home, "bin")
  fs.mkdirSync(binDir, { recursive: true })
  const stub = path.join(binDir, "skulk")
  fs.writeFileSync(stub, "#!/bin/sh\necho 'skulk 0.0.1'\n")
  fs.chmodSync(stub, 0o755)

  const result = await run(["install"], { home, path: `${binDir}:${BARE_PATH}` })
  assert.equal(result.status, 0, result.stderr)
  assert.match(result.stdout, /skulk 0\.0\.1 at/)
  assert.match(result.stdout, new RegExp(`${LATEST} is available`))
  assert.match(result.stdout, /skulk update/)
  assert.equal(fs.existsSync(path.join(home, ".local", "bin", "skulk")), false)
})

test("an up-to-date binary draws no upgrade suggestion", async () => {
  const home = tmpdir("current")
  const binDir = path.join(home, "bin")
  fs.mkdirSync(binDir, { recursive: true })
  const stub = path.join(binDir, "skulk")
  fs.writeFileSync(stub, `#!/bin/sh\necho 'skulk ${LATEST}'\n`)
  fs.chmodSync(stub, 0o755)

  const result = await run(["install"], { home, path: `${binDir}:${BARE_PATH}` })
  assert.equal(result.status, 0, result.stderr)
  assert.doesNotMatch(result.stdout, /is available/)
})

test("x86_64 macOS is refused with install.sh's wording", async () => {
  const home = tmpdir("macos-x86")
  const result = await run(["install"], { home, target: "x86_64:macos" })
  assert.equal(result.status, 1)
  assert.match(result.stderr, /x86_64 macOS is not supported/)
  // The skill still lands; only the binary step fails.
  assert.ok(fs.existsSync(globalSkill(home)))
})

test("--no-binary makes no network calls", async () => {
  const home = tmpdir("offline")
  requested.length = 0
  const result = await run(["install", "--no-binary"], { home })
  assert.equal(result.status, 0, result.stderr)
  assert.deepEqual(requested, [])
})

test("no arguments prints usage and exits 0", async () => {
  const home = tmpdir("usage")
  const result = await run([], { home })
  assert.equal(result.status, 0)
  assert.match(result.stdout, /usage:/)
})

test("an unknown argument exits 1", async () => {
  const home = tmpdir("unknown")
  const result = await run(["install", "--wat"], { home })
  assert.equal(result.status, 1)
  assert.match(result.stderr, /unknown argument: --wat/)
})

test("the packaged skill carries frontmatter OpenCode accepts", async () => {
  const content = fs.readFileSync(SOURCE_SKILL, "utf8")
  const frontmatter = /^---\n([\s\S]*?)\n---/.exec(content)
  assert.ok(frontmatter, "SKILL.md must open with YAML frontmatter")
  // OpenCode requires a string name and, to list the skill at all, a description.
  assert.match(frontmatter[1], new RegExp(`^name: ${SKILL_NAME}$`, "m"))
  assert.match(frontmatter[1], /^description:/m)
})
