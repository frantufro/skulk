use std::path::{Path, PathBuf};

use serde::Deserialize;

pub(crate) const CONFIG_FILENAME: &str = ".skulk.toml";

/// Sample `.skulk.toml` written when no config file exists.
const SAMPLE_CONFIG: &str = r#"# skulk CLI configuration
# Edit these values for your project and server.

host = "your-server"
session_prefix = "skulk-"
base_path = "~/your-project"
worktree_base = "~/your-project-worktrees"
# default_branch = "main"
"#;

/// Runtime configuration loaded from `.skulk.toml`.
///
/// All fields are mandatory. If no config file is found in the current
/// directory or any parent, a sample file is generated in the current
/// directory and the CLI exits with instructions to edit it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Config {
    pub host: String,
    pub session_prefix: String,
    pub base_path: String,
    pub worktree_base: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

/// Validate that a config value contains only shell-safe characters.
///
/// Values are interpolated into shell commands without quoting, so they must not
/// contain spaces, quotes, or other metacharacters.
fn validate_shell_safe(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("'{field}' cannot be empty in {CONFIG_FILENAME}"));
    }
    for c in value.chars() {
        if !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '/' | '.' | '-' | '_' | '~' | '+' | '@' | ':')
        {
            return Err(format!(
                "'{field}' contains character '{c}' that is unsafe in shell commands. \
                 Only alphanumeric characters and /._-~+@: are allowed."
            ));
        }
    }
    Ok(())
}

/// Walks from `start` up to the filesystem root looking for `.skulk.toml`.
fn find_config_file(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Loads configuration from the nearest `.skulk.toml`.
///
/// If no config file is found, generates a sample `.skulk.toml` in `start`
/// and returns an error telling the user to edit it.
///
/// # Errors
///
/// Returns an error if:
/// - No config file exists (after generating a sample)
/// - The config file cannot be read
/// - The config file has invalid TOML or missing fields
pub(crate) fn load_config(start: &Path) -> Result<Config, String> {
    let Some(path) = find_config_file(start) else {
        let generated = start.join(CONFIG_FILENAME);
        return match std::fs::write(&generated, SAMPLE_CONFIG) {
            Ok(()) => Err(format!(
                "No {CONFIG_FILENAME} found. Generated sample at {}.\n\
                 Edit it with your server and project settings, then re-run.",
                generated.display()
            )),
            Err(e) => Err(format!(
                "No {CONFIG_FILENAME} found and could not generate sample at {}: {e}",
                generated.display()
            )),
        };
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let cfg: Config = toml::from_str(&content)
        .map_err(|e| format!("invalid config in {}: {e}", path.display()))?;
    validate_shell_safe(&cfg.host, "host")?;
    validate_shell_safe(&cfg.session_prefix, "session_prefix")?;
    validate_shell_safe(&cfg.base_path, "base_path")?;
    validate_shell_safe(&cfg.worktree_base, "worktree_base")?;
    validate_shell_safe(&cfg.default_branch, "default_branch")?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_toml() -> &'static str {
        r#"
            host = "mars"
            session_prefix = "bot-"
            base_path = "~/other-project"
            worktree_base = "~/other-agents"
            default_branch = "develop"
        "#
    }

    #[test]
    fn config_parses_all_fields() {
        let cfg: Config = toml::from_str(full_toml()).unwrap();
        assert_eq!(cfg.host, "mars");
        assert_eq!(cfg.session_prefix, "bot-");
        assert_eq!(cfg.base_path, "~/other-project");
        assert_eq!(cfg.worktree_base, "~/other-agents");
        assert_eq!(cfg.default_branch, "develop");
    }

    #[test]
    fn config_default_branch_defaults_to_main() {
        let toml_str = r#"
            host = "x"
            session_prefix = "a-"
            base_path = "~/p"
            worktree_base = "~/w"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.default_branch, "main");
    }

    #[test]
    fn config_missing_field_errors() {
        let result: Result<Config, _> = toml::from_str("host = \"x\"");
        assert!(result.is_err());
    }

    #[test]
    fn config_unknown_fields_ignored() {
        let toml_str = r#"
            host = "x"
            session_prefix = "a-"
            base_path = "~/p"
            worktree_base = "~/w"
            unknown_key = 42
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.host, "x");
    }

    #[test]
    fn config_load_generates_sample_when_no_file() {
        let dir = std::env::temp_dir().join("skulk_nogenerate_test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let result = load_config(&dir);
        assert!(result.is_err());

        let generated = dir.join(CONFIG_FILENAME);
        assert!(generated.exists(), "sample config should be generated");
        let content = std::fs::read_to_string(&generated).unwrap();
        assert!(content.contains("host"), "sample should contain host field");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_load_reads_file_from_directory() {
        let dir = std::env::temp_dir().join("skulk_config_test2");
        let _ = std::fs::create_dir_all(&dir);
        let config_path = dir.join(CONFIG_FILENAME);
        std::fs::write(&config_path, full_toml()).unwrap();

        let cfg = load_config(&dir).unwrap();
        assert_eq!(cfg.host, "mars");
        assert_eq!(cfg.session_prefix, "bot-");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_load_walks_up_to_parent() {
        let parent = std::env::temp_dir().join("skulk_parent_test2");
        let child = parent.join("subdir");
        let _ = std::fs::create_dir_all(&child);
        let config_path = parent.join(CONFIG_FILENAME);
        std::fs::write(&config_path, full_toml()).unwrap();

        let cfg = load_config(&child).unwrap();
        assert_eq!(cfg.host, "mars");

        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn config_load_errors_on_invalid_toml() {
        let dir = std::env::temp_dir().join("skulk_bad_toml_test2");
        let _ = std::fs::create_dir_all(&dir);
        let config_path = dir.join(CONFIG_FILENAME);
        std::fs::write(&config_path, "not valid {{{").unwrap();

        let result = load_config(&dir);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── validate_shell_safe tests ──────────────────────────────────────

    #[test]
    fn validate_shell_safe_accepts_typical_path() {
        assert!(validate_shell_safe("~/my-project", "base_path").is_ok());
    }

    #[test]
    fn validate_shell_safe_accepts_complex_path() {
        assert!(validate_shell_safe("/home/user/projects/my_app.v2", "base_path").is_ok());
    }

    #[test]
    fn validate_shell_safe_rejects_space() {
        let result = validate_shell_safe("~/my project", "base_path");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("' '"));
    }

    #[test]
    fn validate_shell_safe_rejects_single_quote() {
        let result = validate_shell_safe("it's", "host");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("'''"));
    }

    #[test]
    fn validate_shell_safe_rejects_semicolon() {
        let result = validate_shell_safe("foo;rm -rf /", "base_path");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("';'"));
    }

    #[test]
    fn validate_shell_safe_rejects_empty() {
        let result = validate_shell_safe("", "host");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot be empty"));
    }

    #[test]
    fn config_load_rejects_path_with_spaces() {
        let dir = std::env::temp_dir().join("skulk_shell_safe_test");
        let _ = std::fs::create_dir_all(&dir);
        let config_path = dir.join(CONFIG_FILENAME);
        std::fs::write(
            &config_path,
            r#"
                host = "server"
                session_prefix = "skulk-"
                base_path = "~/my project"
                worktree_base = "~/agents"
            "#,
        )
        .unwrap();

        let result = load_config(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("base_path"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
