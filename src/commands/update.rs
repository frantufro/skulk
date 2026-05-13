use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::SkulkError;
use anyhow::Context;

/// Cache duration for version check (24 hours)
const CACHE_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

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

/// Check if a newer version is available. Returns `(latest_version, current_version)` if update exists.
pub(crate) fn check_staleness(client: &impl HttpClient) -> Option<(String, String)> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let Ok(latest) = get_latest_version(client) else {
        return None; // Silently skip on fetch errors
    };
    let latest_trimmed = latest.strip_prefix('v').unwrap_or(&latest);
    if latest_trimmed > current.as_str() {
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
}

/// Run the update command
pub(crate) fn cmd_update(client: &impl HttpClient) -> Result<(), SkulkError> {
    let current_version = env!("CARGO_PKG_VERSION");
    let release = client.get_latest_release()?;
    let latest_version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    if latest_version <= current_version {
        println!("skulk is already up to date (v{current_version})");
        return Ok(());
    }

    // Find asset for current platform
    let target = std::env::var("TARGET")
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to get TARGET env var: {e}")))?;
    let asset = release
        .assets
        .iter()
        .find(|a| a.name.contains(&target))
        .ok_or_else(|| {
            SkulkError::UpdateFailed(format!("No release asset found for target {target}"))
        })?;

    // Get current binary path
    let current_exe = std::env::current_exe().map_err(|e| {
        SkulkError::UpdateFailed(format!("Failed to get current executable path: {e}"))
    })?;
    let temp_path = current_exe.with_extension("tmp");

    // Download to temp path
    client.download_asset(&asset.browser_download_url, &temp_path)?;

    // Replace binary atomically
    std::fs::rename(&temp_path, &current_exe)
        .map_err(|e| SkulkError::UpdateFailed(format!("Failed to replace binary: {e}")))?;

    println!("Updated skulk to v{latest_version}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::{Duration, UNIX_EPOCH};

    struct MockHttpClient {
        release: Result<ReleaseInfo, SkulkError>,
        download_result: Result<(), SkulkError>,
    }

    impl HttpClient for MockHttpClient {
        fn get_latest_release(&self) -> Result<ReleaseInfo, SkulkError> {
            self.release.clone()
        }

        fn download_asset(&self, _url: &str, _dest: &Path) -> Result<(), SkulkError> {
            self.download_result.clone()
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
}
