use crate::config::Config;
use crate::error::SkulkError;
use crate::ssh::Ssh;

pub(crate) fn cmd_pull(ssh: &impl Ssh, force: bool, cfg: &Config) -> Result<(), SkulkError> {
    let base_path = &cfg.base_path;
    let host = &cfg.host;
    // Check if the base clone directory exists and is a git repo
    match ssh.run(&format!("test -d {base_path}/.git && echo exists")) {
        Ok(_) => {} // Directory exists and is a git repo
        Err(SkulkError::SshFailed(_)) => {
            return Err(SkulkError::Validation(format!(
                "Base clone not found at {base_path} on {host}.\n\
                 Set it up with: ssh {host} 'git clone <your-repo-url> {base_path}'"
            )));
        }
        Err(e) => return Err(e), // SSH connectivity issue — propagate
    }

    if force {
        eprintln!("Warning: This will discard any local changes on {host}.");
        let output = ssh.run(&format!(
            "cd {base_path} && git fetch origin && git reset --hard origin/main"
        ))?;
        println!("{output}");
    } else {
        match ssh.run(&format!("cd {base_path} && git pull --ff-only origin main")) {
            Ok(output) => {
                println!("{output}");
            }
            Err(SkulkError::SshFailed(ref stderr)) => {
                let lower = stderr.to_lowercase();
                if lower.contains("not possible to fast-forward")
                    || lower.contains("non-fast-forward")
                {
                    return Err(SkulkError::Validation(
                        "Cannot fast-forward. Remote has diverged.\n  \
                         Run `skulk pull --force` to discard local changes and reset to origin/main."
                            .into(),
                    ));
                } else if lower.contains("please commit your changes or stash them") {
                    return Err(SkulkError::Validation(format!(
                        "Working tree has uncommitted changes on {host}.\n  \
                         Commit or stash changes on {host}, then retry."
                    )));
                } else if lower.contains("couldn't find remote ref")
                    || lower.contains("fatal: invalid refspec")
                {
                    return Err(SkulkError::Validation(format!(
                        "Cannot find 'main' branch on origin.\n  \
                         Check remote configuration: ssh {host} 'cd {base_path} && git remote -v'"
                    )));
                }
                return Err(SkulkError::SshFailed(stderr.clone()));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockSsh, test_config};

    #[test]
    fn cmd_pull_normal_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Ok("exists".into()), Ok("Already up to date.".into())]);
        assert!(cmd_pull(&ssh, false, &cfg).is_ok());
    }

    #[test]
    fn cmd_pull_missing_base_clone_returns_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::SshFailed("test failed".into()))]);
        let result = cmd_pull(&ssh, false, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => {
                assert!(msg.contains("Base clone not found"));
            }
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_pull_force_succeeds() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Ok("HEAD is now at abc1234 latest commit".into()),
        ]);
        assert!(cmd_pull(&ssh, true, &cfg).is_ok());
    }

    #[test]
    fn cmd_pull_diverged_returns_validation_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Err(SkulkError::SshFailed(
                "fatal: Not possible to fast-forward, aborting.".into(),
            )),
        ]);
        let result = cmd_pull(&ssh, false, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => {
                assert!(msg.contains("Cannot fast-forward"));
            }
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_pull_uncommitted_changes_returns_validation_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Err(SkulkError::SshFailed(
                "error: Your local changes to the following files would be overwritten by merge:\n\
                 \tfile.rs\nPlease commit your changes or stash them before you merge."
                    .into(),
            )),
        ]);
        let result = cmd_pull(&ssh, false, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => {
                assert!(msg.contains("uncommitted changes"));
            }
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_pull_invalid_refspec_returns_validation_error() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Err(SkulkError::SshFailed(
                "fatal: couldn't find remote ref main".into(),
            )),
        ]);
        let result = cmd_pull(&ssh, false, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Validation(msg) => {
                assert!(msg.contains("Cannot find 'main' branch"));
            }
            other => panic!("expected Validation, got: {other}"),
        }
    }

    #[test]
    fn cmd_pull_generic_ssh_error_propagated() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Err(SkulkError::Diagnostic {
                message: "Connection timed out.".into(),
                suggestion: "Check network.".into(),
            }),
        ]);
        let result = cmd_pull(&ssh, false, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Diagnostic { message, .. } => {
                assert!(message.contains("timed out"));
            }
            other => panic!("expected Diagnostic, got: {other}"),
        }
    }

    #[test]
    fn cmd_pull_base_clone_check_connectivity_error_propagated() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![Err(SkulkError::Diagnostic {
            message: "Connection refused.".into(),
            suggestion: "SSH not running.".into(),
        })]);
        let result = cmd_pull(&ssh, false, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::Diagnostic { message, .. } => {
                assert!(message.contains("refused"));
            }
            other => panic!("expected Diagnostic, got: {other}"),
        }
    }

    #[test]
    fn cmd_pull_unknown_ssh_error_prints_stderr() {
        let cfg = test_config();
        let ssh = MockSsh::new(vec![
            Ok("exists".into()),
            Err(SkulkError::SshFailed(
                "some totally unknown git error".into(),
            )),
        ]);
        let result = cmd_pull(&ssh, false, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            SkulkError::SshFailed(msg) => {
                assert!(msg.contains("some totally unknown git error"));
            }
            other => panic!("expected SshFailed, got: {other}"),
        }
    }

    #[test]
    fn missing_base_clone_error_does_not_reference_skulk_bootstrap() {
        let cfg = test_config();
        let msg = format!(
            "Error: Base clone not found at {} on {}.\n\
             Set it up with: ssh {} 'git clone <your-repo-url> {}'",
            cfg.base_path, cfg.host, cfg.host, cfg.base_path
        );
        assert!(msg.contains("git clone"));
        assert!(!msg.contains("skulk bootstrap"));
    }

    #[test]
    fn force_pull_warning_contains_discard() {
        let cfg = test_config();
        let warning = format!(
            "Warning: This will discard any local changes on {}.",
            cfg.host
        );
        assert!(warning.contains("discard"));
    }
}
