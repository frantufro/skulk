use std::path::{Component, Path, PathBuf};

use crate::agent::Agent;
use crate::config::{Config, OutputFormat};
use crate::display::emit_json;
use crate::error::SkulkError;
use crate::ssh::Ssh;
use crate::util::{shell_escape, shell_quote_path};

/// Default cap on the total bytes one `skulk get` copies, in megabytes.
pub(crate) const DEFAULT_MAX_MB: u64 = 200;

/// Prefix of the stderr line the remote script prints when it refuses the
/// path, reported as a validation error.
const REFUSAL: &str = "skulk-get-refused: ";

/// Prefix of the stderr line the remote script prints when the path does not
/// exist, reported as not found.
const MISSING: &str = "skulk-get-missing: ";

/// First stdout line of a successful walk: `HEADER\t<kind>\t<staging dir>`.
const HEADER: &str = "skulk-get-ok";

/// What `skulk get` was asked to do.
pub(crate) struct GetRequest<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub dest: Option<&'a Path>,
    pub recursive: bool,
    pub list: bool,
    pub max_bytes: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Kind {
    File,
    Dir,
}

/// One file the remote walk accepted. `index` names its staged copy.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    index: usize,
    path: String,
    bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct Listing {
    kind: Kind,
    staging: Option<String>,
    files: Vec<Entry>,
}

/// Copy a file, or a directory's files, out of agent `name`'s worktree, or
/// list them with `--list`.
///
/// One script walks the path on the remote, refusing anything that leaves the
/// worktree through a symlink, and copies the accepted files to a fresh
/// staging directory under safe names (the transfer path is not shell-quoted).
/// Each staged file is then downloaded and the staging directory removed.
pub(crate) fn cmd_get(ssh: &impl Ssh, req: &GetRequest, cfg: &Config) -> Result<(), SkulkError> {
    let agent = Agent::resolve(req.name, cfg)?;
    let rel = normalize_path(req.path)?;
    let worktree = agent.worktree_path(cfg);
    let script = walk_script(
        &worktree,
        &rel,
        req.recursive || req.list,
        !req.list,
        req.max_bytes,
    );
    let out = ssh.run(&script).map_err(|e| script_error(&agent, e))?;
    let Some(mut listing) = parse_listing(&out)? else {
        return Err(SkulkError::NotFound(format!(
            "Agent '{}' has no worktree on {}. Check agents with `skulk list`.",
            req.name, cfg.host
        )));
    };

    let result = check_listing(&listing, &rel, req.max_bytes).and_then(|()| {
        if req.list {
            Ok(None)
        } else {
            copy_out(ssh, &agent, &listing, &rel, req.dest).map(Some)
        }
    });
    if let Some(staging) = &listing.staging {
        let _ = ssh.run(&format!("rm -rf '{}'", shell_escape(staging)));
    }
    let dest = result?;

    listing.files.sort_by(|a, b| a.path.cmp(&b.path));
    if cfg.output_format == OutputFormat::Json {
        emit_json(&json_get_result(req.name, &listing.files, dest.as_deref()));
    } else if let Some(dest) = &dest {
        for entry in &listing.files {
            println!(
                "{}",
                local_target(&listing, &rel, dest, &entry.path).display()
            );
        }
    } else {
        for entry in &listing.files {
            println!("{}\t{}", entry.bytes, entry.path);
        }
    }
    Ok(())
}

/// The `--json` payload; `dest` is `None` for `--list`.
fn json_get_result(name: &str, files: &[Entry], dest: Option<&Path>) -> serde_json::Value {
    let files: Vec<_> = files
        .iter()
        .map(|f| serde_json::json!({"path": f.path, "bytes": f.bytes}))
        .collect();
    serde_json::json!({
        "agent": name,
        "files": files,
        "dest": dest.map(|d| d.to_string_lossy().into_owned()),
    })
}

/// Map a failed remote script: a refusal becomes a validation error, a missing
/// path not found; anything else is an SSH failure on behalf of the agent.
fn script_error(agent: &Agent, err: SkulkError) -> SkulkError {
    if let SkulkError::SshFailed(stderr) = &err {
        for line in stderr.lines() {
            if let Some(msg) = line.strip_prefix(REFUSAL) {
                return SkulkError::Validation(msg.to_string());
            }
            if let Some(msg) = line.strip_prefix(MISSING) {
                return SkulkError::NotFound(msg.to_string());
            }
        }
    }
    agent.classify(err)
}

/// Accept a path relative to the worktree, without `..`, and return it with
/// `.` and empty components dropped. Symlinks are checked on the remote.
fn normalize_path(path: &str) -> Result<String, SkulkError> {
    let reject = |why: &str| {
        Err(SkulkError::Validation(format!(
            "Invalid path '{path}': {why}"
        )))
    };
    if path.chars().any(char::is_control) {
        return reject("it contains a control character.");
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return reject("it must be relative to the agent's worktree.");
    }
    if p.components().any(|c| c == Component::ParentDir) {
        return reject("it must not contain '..'.");
    }
    let parts: Vec<String> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        return reject("it must name a file or directory inside the worktree.");
    }
    Ok(parts.join("/"))
}

/// Shell script that walks `rel` inside `worktree` and prints a [`HEADER`]
/// line, then one `<bytes>\t<path>` line per file, `path` relative to the
/// worktree. Prints nothing when the worktree does not exist.
///
/// Every entry is resolved to its physical path and refused unless it stays
/// inside the worktree, so a symlink leading out fails the whole call; links
/// that stay inside are followed and their targets copied as regular files.
/// With `stage`, the n-th file is copied to `<staging>/<n>`, and the staging
/// directory is removed again on any refusal. The total is checked against
/// `max_bytes` before each copy.
fn walk_script(worktree: &str, rel: &str, recursive: bool, stage: bool, max_bytes: u64) -> String {
    let max = max_bytes.min(i64::MAX.unsigned_abs());
    let rec = u8::from(recursive);
    let stage = u8::from(stage);
    let wt = shell_quote_path(worktree);
    let rel = shell_escape(rel);
    format!(
        "cd {wt} 2>/dev/null || exit 0; W=$(pwd -P) || exit 1; S=\n\
         die() {{ [ -n \"$S\" ] && rm -rf \"$S\"; exit 1; }}\n\
         refuse() {{ echo \"{REFUSAL}$1\" >&2; die; }}\n\
         inside() {{ case \"$1/\" in \"$W\"/*) return 0;; esac; return 1; }}\n\
         p='./{rel}'; q=${{p#./}}\n\
         if [ ! -e \"$p\" ]; then\n\
         [ -L \"$p\" ] && refuse \"$q is a symlink to nothing.\"\n\
         echo \"{MISSING}$q does not exist in the agent's worktree.\" >&2; exit 1\n\
         fi\n\
         if [ -d \"$p\" ]; then\n\
         k=dir; [ {rec} = 1 ] || refuse \"$q is a directory; pass --recursive to copy it.\"\n\
         nl=$(printf '\\nx'); nl=${{nl%x}}\n\
         [ -n \"$(find -L \"$p\" -name \"*$nl*\" -print)\" ] && refuse \"$q holds a file name with a newline.\"\n\
         L=$(find -L \"$p\" -print) || refuse \"could not walk $q (is there a symlink loop?).\"\n\
         else k=file; L=$p; fi\n\
         if [ {stage} = 1 ]; then S=$(mktemp -d \"${{TMPDIR:-/tmp}}/skulk-get.XXXXXX\") || exit 1; fi\n\
         printf '{HEADER}\\t%s\\t%s\\n' \"$k\" \"$S\"\n\
         t=0; i=0\n\
         while IFS= read -r f; do\n\
         [ -z \"$f\" ] && continue\n\
         g=${{f#./}}\n\
         [ -L \"$f\" ] && [ ! -e \"$f\" ] && refuse \"$g is a symlink to nothing.\"\n\
         r=$(readlink -f \"$f\") || die\n\
         inside \"$r\" || refuse \"$g leads outside the agent's worktree.\"\n\
         [ -d \"$f\" ] && continue\n\
         [ -f \"$f\" ] || refuse \"$g is not a regular file.\"\n\
         b=$(wc -c < \"$f\") || die; b=$((b)); t=$((t + b))\n\
         [ \"$t\" -gt {max} ] && refuse \"$q is more than {max} bytes in total; raise the cap with --max-mb.\"\n\
         if [ -n \"$S\" ]; then cp \"$r\" \"$S/$i\" || die; fi\n\
         printf '%s\\t%s\\n' \"$b\" \"$g\"; i=$((i + 1))\n\
         done <<SKULK_GET_EOF\n\
         $L\n\
         SKULK_GET_EOF\n"
    )
}

/// Parse [`walk_script`]'s output; `None` when the worktree does not exist.
fn parse_listing(out: &str) -> Result<Option<Listing>, SkulkError> {
    let malformed = |line: &str| {
        SkulkError::SshFailed(format!("unexpected output from the remote walk: {line:?}"))
    };
    let mut lines = out.lines();
    let Some(header) = lines.next().filter(|l| !l.trim().is_empty()) else {
        return Ok(None);
    };
    let mut fields = header.splitn(3, '\t');
    if fields.next() != Some(HEADER) {
        return Err(malformed(header));
    }
    let kind = match fields.next() {
        Some("file") => Kind::File,
        Some("dir") => Kind::Dir,
        _ => return Err(malformed(header)),
    };
    let staging = match fields.next() {
        Some("") => None,
        Some(s) if is_safe_staging(s) => Some(s.to_string()),
        _ => return Err(malformed(header)),
    };
    let files = lines
        .enumerate()
        .map(|(index, line)| {
            let (bytes, path) = line.split_once('\t').ok_or_else(|| malformed(line))?;
            let bytes = bytes.parse().map_err(|_| malformed(line))?;
            Ok(Entry {
                index,
                path: path.to_string(),
                bytes,
            })
        })
        .collect::<Result<_, SkulkError>>()?;
    Ok(Some(Listing {
        kind,
        staging,
        files,
    }))
}

/// The staging directory is passed to the transfer unquoted, so it must be an
/// absolute path of shell-safe characters.
fn is_safe_staging(s: &str) -> bool {
    s.starts_with('/')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
}

/// Check the remote's answer before anything is written locally: every path
/// lies under `rel` without `..`, and the total fits the cap.
fn check_listing(listing: &Listing, rel: &str, max_bytes: u64) -> Result<(), SkulkError> {
    for entry in &listing.files {
        let suffix = relative_suffix(listing.kind, rel, &entry.path);
        let ok = suffix.is_some_and(|s| {
            Path::new(s)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        });
        if !ok {
            return Err(SkulkError::SshFailed(format!(
                "the remote walk reported '{}', which is not under '{rel}'.",
                entry.path
            )));
        }
    }
    let total: u64 = listing.files.iter().map(|f| f.bytes).sum();
    if total > max_bytes {
        return Err(SkulkError::Validation(format!(
            "{rel} is more than {max_bytes} bytes in total; raise the cap with --max-mb."
        )));
    }
    Ok(())
}

/// `path` relative to the requested directory, or `""` for the requested file.
fn relative_suffix<'a>(kind: Kind, rel: &str, path: &'a str) -> Option<&'a str> {
    match kind {
        Kind::File => (path == rel).then_some(""),
        Kind::Dir => path.strip_prefix(rel)?.strip_prefix('/'),
    }
}

/// Download every staged file and return the absolute local destination.
fn copy_out(
    ssh: &impl Ssh,
    agent: &Agent,
    listing: &Listing,
    rel: &str,
    dest: Option<&Path>,
) -> Result<PathBuf, SkulkError> {
    let staging = listing
        .staging
        .as_deref()
        .ok_or_else(|| SkulkError::SshFailed("the remote walk staged no files to copy.".into()))?;
    let root = local_root(listing.kind, rel, dest);
    let root = std::path::absolute(&root).map_err(|e| local_error(&root, &e))?;
    if listing.kind == Kind::Dir {
        std::fs::create_dir_all(&root).map_err(|e| local_error(&root, &e))?;
    }
    for entry in &listing.files {
        let target = local_target(listing, rel, &root, &entry.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| local_error(parent, &e))?;
        }
        ssh.download_file(&format!("{staging}/{}", entry.index), &target)
            .map_err(|e| agent.classify(e))?;
    }
    Ok(root)
}

fn local_error(path: &Path, err: &std::io::Error) -> SkulkError {
    SkulkError::Validation(format!("Cannot write '{}': {err}", path.display()))
}

/// Where the requested path lands locally. A file goes to `dest`, or into it
/// when it is an existing directory; a directory's contents go into `dest`.
/// Without `dest`, both land in the current directory under their own name.
fn local_root(kind: Kind, rel: &str, dest: Option<&Path>) -> PathBuf {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    match (kind, dest) {
        (_, None) => PathBuf::from(base),
        (Kind::File, Some(d)) if d.is_dir() => d.join(base),
        (_, Some(d)) => d.to_path_buf(),
    }
}

fn local_target(listing: &Listing, rel: &str, root: &Path, path: &str) -> PathBuf {
    match relative_suffix(listing.kind, rel, path) {
        Some(suffix) if !suffix.is_empty() => root.join(suffix),
        _ => root.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, assert_err, ssh_err, test_config, unique_temp_path};

    fn request<'a>(path: &'a str, dest: Option<&'a Path>) -> GetRequest<'a> {
        GetRequest {
            name: "my-agent",
            path,
            dest,
            recursive: false,
            list: false,
            max_bytes: 1000,
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = unique_temp_path(prefix);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir.canonicalize().expect("canonicalize")
    }

    // ── cmd_get ─────────────────────────────────────────────────────────

    #[test]
    fn downloads_staged_file_then_removes_staging() {
        let dir = temp_dir("skulk_get_dest");
        let dest = dir.join("shot.png");
        let ssh = MockSsh::new(vec![
            Ok(format!(
                "{HEADER}\tfile\t/tmp/skulk-get.abc\n5\ttmp/shot.png\n"
            )),
            Ok(String::new()),
        ]);

        let result = cmd_get(&ssh, &request("tmp/shot.png", Some(&dest)), &test_config());

        assert!(result.is_ok(), "{result:?}");
        let calls = ssh.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[1],
            format!("DOWNLOAD /tmp/skulk-get.abc/0:{}", dest.display())
        );
        assert_eq!(calls[2], "rm -rf '/tmp/skulk-get.abc'");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_downloads_nothing() {
        let ssh = MockSsh::new(vec![Ok(format!("{HEADER}\tdir\t\n5\tmedia/a.png\n"))]);
        let mut req = request("media", None);
        req.list = true;

        let result = cmd_get(&ssh, &req, &test_config());

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(ssh.calls().len(), 1);
    }

    #[test]
    fn returns_not_found_when_agent_has_no_worktree() {
        let ssh = MockSsh::new(vec![Ok(String::new())]);

        let result = cmd_get(&ssh, &request("a.png", None), &test_config());

        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.contains("my-agent"), "{msg}");
        });
    }

    #[test]
    fn reports_missing_path_as_not_found() {
        let ssh = MockSsh::new(vec![ssh_err(&format!(
            "{MISSING}a.png does not exist in the agent's worktree."
        ))]);

        let result = cmd_get(&ssh, &request("a.png", None), &test_config());

        assert_err!(result, SkulkError::NotFound(msg) => {
            assert!(msg.starts_with("a.png"), "{msg}");
        });
    }

    #[test]
    fn reports_refusal_as_validation_error() {
        let ssh = MockSsh::new(vec![ssh_err(&format!(
            "{REFUSAL}docs leads outside the agent's worktree."
        ))]);

        let result = cmd_get(&ssh, &request("docs/x", None), &test_config());

        assert_err!(result, SkulkError::Validation(msg) => {
            assert_eq!(msg, "docs leads outside the agent's worktree.");
        });
    }

    #[test]
    fn refuses_escaping_path_without_contacting_the_remote() {
        let ssh = MockSsh::new(vec![]);

        let result = cmd_get(&ssh, &request("../x", None), &test_config());

        assert_err!(result, SkulkError::Validation(_) => {});
        assert_eq!(ssh.calls(), Vec::<String>::new());
    }

    #[test]
    fn refuses_reported_path_outside_the_request_and_cleans_up() {
        let ssh = MockSsh::new(vec![
            Ok(format!("{HEADER}\tdir\t/tmp/skulk-get.abc\n5\tother/x\n")),
            Ok(String::new()),
        ]);
        let mut req = request("media", None);
        req.recursive = true;

        let result = cmd_get(&ssh, &req, &test_config());

        assert!(result.is_err());
        let calls = ssh.calls();
        assert_eq!(calls.len(), 2, "nothing downloaded: {calls:?}");
        assert_eq!(calls[1], "rm -rf '/tmp/skulk-get.abc'");
    }

    #[test]
    fn json_get_result_lists_files_and_dest() {
        let files = vec![Entry {
            index: 0,
            path: "tmp/a.png".into(),
            bytes: 3,
        }];

        let value = json_get_result("my-agent", &files, Some(Path::new("/l/a.png")));

        assert_eq!(
            value,
            serde_json::json!({
                "agent": "my-agent",
                "files": [{"path": "tmp/a.png", "bytes": 3}],
                "dest": "/l/a.png",
            })
        );
    }

    // ── normalize_path / parse_listing / local_root ─────────────────────

    #[test]
    fn normalizes_dot_and_empty_components() {
        assert_eq!(
            normalize_path("./tmp//review-media/").ok().as_deref(),
            Some("tmp/review-media")
        );
    }

    #[test]
    fn rejects_paths_outside_or_naming_nothing() {
        for bad in ["", ".", "/etc/passwd", "../x", "a/../../x", "a\nb"] {
            assert!(normalize_path(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn parses_header_and_entries_with_tabs_in_names() {
        let out = format!("{HEADER}\tdir\t/tmp/s.1\n3\td/a b\tc\n0\td/e\n");

        let listing = parse_listing(&out).expect("parse").expect("some");

        assert_eq!(listing.kind, Kind::Dir);
        assert_eq!(listing.staging.as_deref(), Some("/tmp/s.1"));
        assert_eq!(
            listing.files[0],
            Entry {
                index: 0,
                path: "d/a b\tc".into(),
                bytes: 3
            }
        );
        assert_eq!(listing.files[1].index, 1);
    }

    #[test]
    fn rejects_unsafe_staging_dir() {
        let out = format!("{HEADER}\tfile\t/tmp/x;rm -rf ~\n");

        assert!(parse_listing(&out).is_err());
    }

    #[test]
    fn local_root_defaults_to_the_base_name() {
        assert_eq!(
            local_root(Kind::Dir, "tmp/review-media", None),
            PathBuf::from("review-media")
        );
    }

    #[test]
    fn local_root_puts_a_file_inside_an_existing_directory() {
        let dir = temp_dir("skulk_get_root");

        assert_eq!(
            local_root(Kind::File, "tmp/a.png", Some(&dir)),
            dir.join("a.png")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── walk_script, run for real ───────────────────────────────────────

    fn run_walk(wt: &Path, rel: &str, recursive: bool, stage: bool, max: u64) -> Output {
        let script = walk_script("WT", rel, recursive, stage, max).replacen(
            "cd 'WT'",
            &format!("cd '{}'", wt.display()),
            1,
        );
        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .expect("run sh");
        Output {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    struct Output {
        ok: bool,
        stdout: String,
        stderr: String,
    }

    fn listing(out: &Output) -> Listing {
        assert!(out.ok, "{}", out.stderr);
        parse_listing(&out.stdout).expect("parse").expect("some")
    }

    fn worktree_with_media() -> PathBuf {
        let wt = temp_dir("skulk_get_wt");
        std::fs::create_dir_all(wt.join("media/sub")).expect("mkdir");
        std::fs::write(wt.join("media/a.png"), b"aaa").expect("write");
        std::fs::write(wt.join("media/sub/b c.png"), b"bb").expect("write");
        wt
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("symlink");
    }

    #[test]
    fn walk_stages_a_directory_and_reports_sizes() {
        let wt = worktree_with_media();

        let l = listing(&run_walk(&wt, "media", true, true, 1000));

        let mut paths: Vec<_> = l.files.iter().map(|f| (f.path.as_str(), f.bytes)).collect();
        paths.sort_unstable();
        assert_eq!(paths, [("media/a.png", 3), ("media/sub/b c.png", 2)]);
        let staging = PathBuf::from(l.staging.expect("staged"));
        for f in &l.files {
            let staged = std::fs::read(staging.join(f.index.to_string())).expect("staged");
            assert_eq!(staged, std::fs::read(wt.join(&f.path)).expect("source"));
        }
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn walk_without_stage_creates_no_staging_dir() {
        let wt = worktree_with_media();

        let l = listing(&run_walk(&wt, "media", true, false, 1000));

        assert_eq!(l.staging, None);
        assert_eq!(l.files.len(), 2);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn walk_refuses_directory_without_recursive() {
        let wt = worktree_with_media();

        let out = run_walk(&wt, "media", false, true, 1000);

        assert!(!out.ok);
        assert!(out.stderr.contains("--recursive"), "{}", out.stderr);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn walk_reports_missing_path() {
        let wt = worktree_with_media();

        let out = run_walk(&wt, "nope.png", false, true, 1000);

        assert!(!out.ok);
        assert!(out.stderr.contains(MISSING), "{}", out.stderr);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn walk_prints_nothing_when_worktree_is_missing() {
        let wt = unique_temp_path("skulk_get_missing");

        let out = run_walk(&wt, "a", false, true, 1000);

        assert!(out.ok, "{}", out.stderr);
        assert_eq!(out.stdout, "");
    }

    #[test]
    fn walk_refuses_over_the_cap_and_removes_staging() {
        let wt = worktree_with_media();
        let tmp = temp_dir("skulk_get_tmpdir");
        let script = walk_script("WT", "media", true, true, 4).replacen(
            "cd 'WT'",
            &format!("cd '{}'", wt.display()),
            1,
        );

        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .env("TMPDIR", &tmp)
            .output()
            .expect("run sh");

        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--max-mb"),
            "{out:?}"
        );
        assert_eq!(std::fs::read_dir(&tmp).expect("tmp").count(), 0);
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[cfg(unix)]
    #[test]
    fn walk_copies_symlinks_that_stay_inside_as_regular_files() {
        let wt = worktree_with_media();
        std::fs::write(wt.join("elsewhere.txt"), b"inside").expect("write");
        symlink(&wt.join("elsewhere.txt"), &wt.join("media/link.txt"));

        let l = listing(&run_walk(&wt, "media", true, true, 1000));

        let link = l
            .files
            .iter()
            .find(|f| f.path == "media/link.txt")
            .expect("link listed");
        let staging = PathBuf::from(l.staging.expect("staged"));
        let staged = staging.join(link.index.to_string());
        assert!(!staged.is_symlink());
        assert_eq!(std::fs::read(&staged).expect("staged"), b"inside");
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[cfg(unix)]
    #[test]
    fn walk_refuses_directory_holding_a_symlink_leading_outside() {
        let wt = worktree_with_media();
        let outside = temp_dir("skulk_get_outside");
        std::fs::write(outside.join("secret"), b"x").expect("write");
        symlink(&outside.join("secret"), &wt.join("media/sub/leak"));

        let out = run_walk(&wt, "media", true, true, 1000);

        assert!(!out.ok);
        assert!(out.stderr.contains(REFUSAL), "{}", out.stderr);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn walk_refuses_path_through_a_symlinked_directory_leading_outside() {
        let wt = worktree_with_media();
        let outside = temp_dir("skulk_get_outside");
        std::fs::write(outside.join("secret"), b"x").expect("write");
        symlink(&outside, &wt.join("docs"));

        let out = run_walk(&wt, "docs/secret", false, true, 1000);

        assert!(!out.ok);
        assert!(out.stderr.contains(REFUSAL), "{}", out.stderr);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
