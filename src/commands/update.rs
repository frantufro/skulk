use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::SkulkError;
use anyhow::Context;

/// Cache duration for version check (24 hours)
const CACHE_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// Total timeout for HTTP requests in the update flow.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Get the cache file path: ~/.cache/skulk/latest-version
fn cache_file_path() -> Result<std::path::PathBuf, SkulkError> {
    // Use test-specific cache dir if set, to avoid test interference
    let cache_dir = if let Ok(test_dir) = std::env::var("SKULK_TEST_CACHE_DIR") {
        std::path::PathBuf::from(test_dir).join("skulk")
    } else {
        dirs::cache_dir()
            .ok_or_else(|| SkulkError::UpdateFailed("Failed to get cache directory".into()))?
            .join("skulk")
    };
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to create cache dir: {e}")))?;
    Ok(cache_dir.join("latest-version"))
}

/// Read cached version and timestamp. Returns `(version, timestamp)` if valid and fresh.
fn read_cache() -> Option<(String, SystemTime)> {
    let Ok(path) = cache_file_path() else {
        return None;
    };
    let content = std::fs::read_to_string(&path).ok()?;
    let mut lines = content.lines();
    let version = lines.next()?.to_string();
    let timestamp_secs: u64 = lines.next()?.parse().ok()?;
    let timestamp = UNIX_EPOCH + Duration::from_secs(timestamp_secs);
    Some((version, timestamp))
}

/// Write version and current timestamp to cache
fn write_cache(version: &str) -> Result<(), SkulkError> {
    let path = cache_file_path()?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| SkulkError::UpdateFailed(format!("System time error: {e}")))?
        .as_secs();
    let content = format!("{version}\n{timestamp}");
    let mut file = std::fs::File::create(&path)
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to create cache file: {e}")))?;
    file.write_all(content.as_bytes())
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to write cache: {e}")))?;
    Ok(())
}

/// Parse a semantic version, accepting an optional leading `v`.
fn parse_version(s: &str) -> Result<semver::Version, semver::Error> {
    semver::Version::parse(s.strip_prefix('v').unwrap_or(s))
}

/// Check if a newer version is available. Returns `(latest_version, current_version)` if update exists.
pub(crate) fn check_staleness(client: &impl HttpClient) -> Option<(String, String)> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let Ok(current_ver) = parse_version(&current) else {
        return None;
    };
    let Ok(latest) = get_latest_version(client) else {
        return None; // Silently skip on fetch errors
    };
    let Ok(latest_ver) = parse_version(&latest) else {
        return None;
    };
    if latest_ver > current_ver {
        Some((latest, current))
    } else {
        None
    }
}

/// Get latest version, using cache if fresh, else fetch and cache.
fn get_latest_version(client: &impl HttpClient) -> Result<String, SkulkError> {
    // Check cache first
    if let Some((cached_version, timestamp)) = read_cache()
        && timestamp.elapsed().unwrap_or(CACHE_DURATION) < CACHE_DURATION
    {
        return Ok(cached_version);
    }
    // Fetch fresh
    let release = client.get_latest_release()?;
    let version = release.tag_name;
    write_cache(&version)?;
    Ok(version)
}

/// Trait for HTTP client to allow mocking in tests
pub(crate) trait HttpClient {
    /// Fetch latest release info from GitHub
    fn get_latest_release(&self) -> Result<ReleaseInfo, SkulkError>;
    /// Download asset from URL to destination path
    fn download_asset(&self, url: &str, dest: &Path) -> Result<(), SkulkError>;
    /// Fetch a small text asset (e.g. a `.sha256` file) directly into memory.
    fn get_text(&self, url: &str) -> Result<String, SkulkError>;
}

/// Info extracted from GitHub latest release response
#[derive(Clone)]
pub(crate) struct ReleaseInfo {
    pub(crate) tag_name: String,
    pub(crate) assets: Vec<ReleaseAsset>,
}

/// Asset info from GitHub release
#[derive(Clone)]
pub(crate) struct ReleaseAsset {
    pub(crate) name: String,
    pub(crate) browser_download_url: String,
}

/// Production HTTP client using ureq
pub(crate) struct UreqClient;

impl HttpClient for UreqClient {
    fn get_latest_release(&self) -> Result<ReleaseInfo, SkulkError> {
        let url = "https://api.github.com/repos/frantufro/skulk/releases/latest";
        let resp = ureq::get(url)
            .set("User-Agent", "skulk")
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("Failed to fetch release from GitHub")
            .map_err(|e| SkulkError::UpdateFailed(e.to_string()))?;
        let json: serde_json::Value = resp
            .into_json()
            .context("Failed to parse release JSON")
            .map_err(|e| SkulkError::UpdateFailed(e.to_string()))?;
        let tag_name = json["tag_name"]
            .as_str()
            .ok_or_else(|| SkulkError::UpdateFailed("Missing tag_name in release response".into()))?
            .to_string();
        let assets = json["assets"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|a| {
                Some(ReleaseAsset {
                    name: a["name"].as_str()?.to_string(),
                    browser_download_url: a["browser_download_url"].as_str()?.to_string(),
                })
            })
            .collect();
        Ok(ReleaseInfo { tag_name, assets })
    }

    fn download_asset(&self, url: &str, dest: &Path) -> Result<(), SkulkError> {
        let resp = ureq::get(url)
            .set("User-Agent", "skulk")
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("Failed to download asset")
            .map_err(|e| SkulkError::UpdateFailed(e.to_string()))?;
        let mut file = std::fs::File::create(dest)
            .context("Failed to create temp file")
            .map_err(|e| SkulkError::UpdateFailed(e.to_string()))?;
        std::io::copy(&mut resp.into_reader(), &mut file)
            .context("Failed to write to temp file")
            .map_err(|e| SkulkError::UpdateFailed(e.to_string()))?;
        Ok(())
    }

    fn get_text(&self, url: &str) -> Result<String, SkulkError> {
        let resp = ureq::get(url)
            .set("User-Agent", "skulk")
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("Failed to fetch text asset")
            .map_err(|e| SkulkError::UpdateFailed(e.to_string()))?;
        resp.into_string()
            .context("Failed to read response body")
            .map_err(|e| SkulkError::UpdateFailed(e.to_string()))
    }
}

/// Compute the SHA-256 of a file as a lowercase hex string.
fn sha256_hex(file_path: &Path) -> Result<String, SkulkError> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut file = std::fs::File::open(file_path)
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to open file for hashing: {e}")))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to hash file: {e}")))?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing to a String can't fail; the unused Result is discarded explicitly.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Extract the hex digest from a sha256sum-style file (`<hash>  <filename>` or just `<hash>`).
fn parse_sha256_file(content: &str) -> Option<String> {
    let token = content.split_whitespace().next()?;
    if token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(token.to_lowercase())
    } else {
        None
    }
}

/// Verify that `file_path` hashes to `expected_sha256` (lowercase hex).
fn verify_checksum(file_path: &Path, expected_sha256: &str) -> Result<(), SkulkError> {
    let computed = sha256_hex(file_path)?;
    let expected = expected_sha256.trim().to_lowercase();
    if computed == expected {
        Ok(())
    } else {
        Err(SkulkError::UpdateFailed(format!(
            "Checksum mismatch (expected {expected}, got {computed})"
        )))
    }
}

/// Run the update command
pub(crate) fn cmd_update(client: &impl HttpClient) -> Result<(), SkulkError> {
    let current_version = env!("CARGO_PKG_VERSION");
    let current_ver = parse_version(current_version)
        .map_err(|e| SkulkError::UpdateFailed(format!("Invalid current version: {e}")))?;
    let release = client.get_latest_release()?;
    let latest_ver = parse_version(&release.tag_name)
        .map_err(|e| SkulkError::UpdateFailed(format!("Invalid latest version: {e}")))?;

    if latest_ver <= current_ver {
        println!("skulk is already up to date (v{current_version})");
        return Ok(());
    }

    // Find asset for current platform
    let target = env!("TARGET");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name.contains(target) && a.name.ends_with(".tar.gz"))
        .ok_or_else(|| {
            SkulkError::UpdateFailed(format!("No release asset found for target {target}"))
        })?;

    // Locate the matching checksum asset; required for safe install.
    let checksum_name = format!("{}.sha256", asset.name);
    let checksum_asset = release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .ok_or_else(|| {
            SkulkError::UpdateFailed(format!(
                "Missing checksum asset {checksum_name} for release {}",
                release.tag_name
            ))
        })?;

    // Get current binary path
    let current_exe = std::env::current_exe().map_err(|e| {
        SkulkError::UpdateFailed(format!("Failed to get current executable path: {e}"))
    })?;
    let archive_path = current_exe.with_extension("tar.gz.tmp");
    let new_binary_path = current_exe.with_extension("new");

    // Download the release tarball.
    client.download_asset(&asset.browser_download_url, &archive_path)?;

    // Verify SHA-256 before extracting; remove the temp file on mismatch so we
    // don't leave a partially-trusted archive on disk.
    let checksum_content = client.get_text(&checksum_asset.browser_download_url)?;
    let expected = parse_sha256_file(&checksum_content).ok_or_else(|| {
        SkulkError::UpdateFailed(format!(
            "Could not parse checksum file {checksum_name} (expected sha256sum format)"
        ))
    })?;
    if let Err(e) = verify_checksum(&archive_path, &expected) {
        let _ = std::fs::remove_file(&archive_path);
        return Err(e);
    }

    // Extract the `skulk` binary out of the tarball and drop the archive.
    if let Err(e) = extract_binary(&archive_path, "skulk", &new_binary_path) {
        let _ = std::fs::remove_file(&archive_path);
        let _ = std::fs::remove_file(&new_binary_path);
        return Err(e);
    }
    let _ = std::fs::remove_file(&archive_path);

    // Restore the executable bit on Unix; rename within the same directory is
    // atomic on POSIX, so the active binary is never observed in a half-written
    // state.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_binary_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| {
                SkulkError::UpdateFailed(format!("Failed to set executable permissions: {e}"))
            })?;
    }

    std::fs::rename(&new_binary_path, &current_exe)
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to replace binary: {e}")))?;

    println!("Updated skulk to v{latest_ver}");
    Ok(())
}

/// Extract a single file named `binary_name` from a gzipped tar archive into `dest`.
fn extract_binary(archive_path: &Path, binary_name: &str, dest: &Path) -> Result<(), SkulkError> {
    let archive_file = std::fs::File::open(archive_path)
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to open archive: {e}")))?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to read archive: {e}")))?;
    for entry in entries {
        let mut entry = entry
            .map_err(|e| SkulkError::UpdateFailed(format!("Failed to read archive entry: {e}")))?;
        // Match on the basename only — we trust the fixed `dest` path, not any
        // path components inside the tarball (no path traversal risk).
        let is_match = entry
            .path()
            .map_err(|e| SkulkError::UpdateFailed(format!("Failed to read entry path: {e}")))?
            .file_name()
            .and_then(|n| n.to_str())
            == Some(binary_name);
        if is_match {
            let mut out = std::fs::File::create(dest).map_err(|e| {
                SkulkError::UpdateFailed(format!("Failed to create extracted binary: {e}"))
            })?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| SkulkError::UpdateFailed(format!("Failed to extract binary: {e}")))?;
            return Ok(());
        }
    }
    Err(SkulkError::UpdateFailed(format!(
        "Archive does not contain a `{binary_name}` binary"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::{Duration, UNIX_EPOCH};

    struct MockHttpClient {
        release: Result<ReleaseInfo, SkulkError>,
        download_result: Result<(), SkulkError>,
        text_result: Result<String, SkulkError>,
    }

    impl HttpClient for MockHttpClient {
        fn get_latest_release(&self) -> Result<ReleaseInfo, SkulkError> {
            self.release.clone()
        }

        fn download_asset(&self, _url: &str, _dest: &Path) -> Result<(), SkulkError> {
            self.download_result.clone()
        }

        fn get_text(&self, _url: &str) -> Result<String, SkulkError> {
            self.text_result.clone()
        }
    }

    #[test]
    #[serial]
    fn update_prints_up_to_date_when_current_is_latest() {
        let release = ReleaseInfo {
            tag_name: "v0.4.2".into(),
            assets: vec![],
        };
        let client = MockHttpClient {
            release: Ok(release),
            download_result: Ok(()),
            text_result: Ok(String::new()),
        };
        let result = cmd_update(&client);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn check_staleness_returns_none_when_current_is_latest() {
        let test_dir =
            std::env::temp_dir().join(format!("skulk_test_cache_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::set_var("SKULK_TEST_CACHE_DIR", test_dir.to_str().unwrap());
        }
        let release = ReleaseInfo {
            tag_name: "v0.4.2".into(),
            assets: vec![],
        };
        let client = MockHttpClient {
            release: Ok(release),
            download_result: Ok(()),
            text_result: Ok(String::new()),
        };
        let result = check_staleness(&client);
        assert!(result.is_none());
        let _ = std::fs::remove_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::remove_var("SKULK_TEST_CACHE_DIR");
        }
    }

    #[test]
    #[serial]
    fn check_staleness_returns_update_when_newer_available() {
        let test_dir =
            std::env::temp_dir().join(format!("skulk_test_cache_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::set_var("SKULK_TEST_CACHE_DIR", test_dir.to_str().unwrap());
        }
        let release = ReleaseInfo {
            tag_name: "v0.4.3".into(),
            assets: vec![],
        };
        let client = MockHttpClient {
            release: Ok(release),
            download_result: Ok(()),
            text_result: Ok(String::new()),
        };
        let result = check_staleness(&client);
        assert!(result.is_some());
        let (latest, current) = result.unwrap();
        assert_eq!(latest, "v0.4.3");
        assert_eq!(current, env!("CARGO_PKG_VERSION"));
        let _ = std::fs::remove_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::remove_var("SKULK_TEST_CACHE_DIR");
        }
    }

    #[test]
    #[serial]
    fn check_staleness_returns_none_on_fetch_error() {
        let test_dir =
            std::env::temp_dir().join(format!("skulk_test_cache_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::set_var("SKULK_TEST_CACHE_DIR", test_dir.to_str().unwrap());
        }
        let client = MockHttpClient {
            release: Err(SkulkError::UpdateFailed("test error".into())),
            download_result: Ok(()),
            text_result: Ok(String::new()),
        };
        let result = check_staleness(&client);
        assert!(result.is_none());
        let _ = std::fs::remove_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::remove_var("SKULK_TEST_CACHE_DIR");
        }
    }

    #[test]
    #[serial]
    fn check_staleness_handles_unparseable_versions() {
        let test_dir =
            std::env::temp_dir().join(format!("skulk_test_cache_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::set_var("SKULK_TEST_CACHE_DIR", test_dir.to_str().unwrap());
        }
        let release = ReleaseInfo {
            tag_name: "not-a-version".into(),
            assets: vec![],
        };
        let client = MockHttpClient {
            release: Ok(release),
            download_result: Ok(()),
            text_result: Ok(String::new()),
        };
        let result = check_staleness(&client);
        assert!(result.is_none());
        let _ = std::fs::remove_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::remove_var("SKULK_TEST_CACHE_DIR");
        }
    }

    #[test]
    fn check_staleness_uses_semver_not_lexicographic() {
        // Lexicographic comparison would say "0.10.0" < "0.9.0"; semver must not.
        let older = parse_version("v0.9.0").unwrap();
        let newer = parse_version("v0.10.0").unwrap();
        assert!(newer > older);
    }

    #[test]
    fn parse_sha256_file_accepts_sha256sum_format() {
        let content =
            "abc123def456abc123def456abc123def456abc123def456abc123def456abcd  skulk.tar.gz\n";
        let parsed = parse_sha256_file(content).unwrap();
        assert_eq!(parsed.len(), 64);
        assert!(parsed.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_sha256_file_accepts_hash_only() {
        let content = "abc123def456abc123def456abc123def456abc123def456abc123def456abcd\n";
        let parsed = parse_sha256_file(content).unwrap();
        assert_eq!(parsed.len(), 64);
    }

    #[test]
    fn parse_sha256_file_rejects_garbage() {
        assert!(parse_sha256_file("nope").is_none());
        assert!(parse_sha256_file("").is_none());
        assert!(parse_sha256_file("abc").is_none());
    }

    #[test]
    fn verify_checksum_accepts_matching_hash() {
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b16...
        let tmp = std::env::temp_dir().join(format!("skulk_chk_{}", rand::random::<u32>()));
        std::fs::write(&tmp, b"hello").unwrap();
        let result = verify_checksum(
            &tmp,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        );
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_ok(), "verify_checksum failed: {result:?}");
    }

    #[test]
    fn verify_checksum_rejects_mismatched_hash() {
        let tmp = std::env::temp_dir().join(format!("skulk_chk_{}", rand::random::<u32>()));
        std::fs::write(&tmp, b"hello").unwrap();
        let result = verify_checksum(
            &tmp,
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let _ = std::fs::remove_file(&tmp);
        assert!(matches!(result, Err(SkulkError::UpdateFailed(_))));
    }

    #[test]
    #[serial]
    fn cache_write_and_read() {
        let test_dir =
            std::env::temp_dir().join(format!("skulk_test_cache_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::set_var("SKULK_TEST_CACHE_DIR", test_dir.to_str().unwrap());
        }
        let version = "v0.4.3";
        let path = cache_file_path().unwrap();
        let _ = std::fs::remove_file(&path); // Clean up before
        write_cache(version).unwrap();
        let cached = read_cache();
        assert!(cached.is_some());
        let (cached_version, timestamp) = cached.unwrap();
        assert_eq!(cached_version, version);
        assert!(timestamp.elapsed().unwrap() < Duration::from_secs(10));
        let _ = std::fs::remove_file(&path); // Clean up after
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::remove_var("SKULK_TEST_CACHE_DIR");
        }
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    #[serial]
    fn cache_expired_after_24h() {
        let test_dir =
            std::env::temp_dir().join(format!("skulk_test_cache_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&test_dir);
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::set_var("SKULK_TEST_CACHE_DIR", test_dir.to_str().unwrap());
        }
        let version = "v0.4.3";
        let path = cache_file_path().unwrap();
        let _ = std::fs::remove_file(&path); // Clean up before
        write_cache(version).unwrap();
        // Simulate expired cache by modifying the timestamp
        let content = format!(
            "{version}\n{}",
            UNIX_EPOCH.elapsed().unwrap().as_secs() - CACHE_DURATION.as_secs() - 1
        );
        std::fs::write(&path, content).unwrap();
        let cached = read_cache();
        assert!(cached.is_some());
        let (_, timestamp) = cached.unwrap();
        assert!(timestamp.elapsed().unwrap() > CACHE_DURATION);
        let _ = std::fs::remove_file(&path); // Clean up after
        // SAFETY: #[serial] ensures no concurrent env access from other tests.
        unsafe {
            std::env::remove_var("SKULK_TEST_CACHE_DIR");
        }
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    /// Build an in-memory gzipped tarball containing one file with the given
    /// name and payload, written to `dest`.
    fn build_test_tarball(dest: &Path, name: &str, payload: &[u8]) {
        let file = std::fs::File::create(dest).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, payload).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn extract_binary_pulls_named_entry_from_tarball() {
        use std::io::Read;
        let tmp = std::env::temp_dir().join(format!("skulk_extract_{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp).unwrap();
        let archive = tmp.join("release.tar.gz");
        let dest = tmp.join("skulk.new");
        let payload: &[u8] = b"\x7fELF fake binary contents";
        build_test_tarball(&archive, "skulk", payload);

        extract_binary(&archive, "skulk", &dest).unwrap();

        let mut got = Vec::new();
        std::fs::File::open(&dest)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, payload);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_binary_errors_when_entry_missing() {
        let tmp = std::env::temp_dir().join(format!("skulk_extract_{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp).unwrap();
        let archive = tmp.join("release.tar.gz");
        let dest = tmp.join("skulk.new");
        build_test_tarball(&archive, "something-else", b"nope");

        let result = extract_binary(&archive, "skulk", &dest);
        assert!(matches!(result, Err(SkulkError::UpdateFailed(_))));
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
