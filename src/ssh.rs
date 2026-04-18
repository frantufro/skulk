use std::path::Path;

use crate::error::SkulkError;

pub(crate) trait Ssh {
    fn run(&self, cmd: &str) -> Result<String, SkulkError>;
    fn interactive(&self, cmd: &str) -> Result<std::process::ExitStatus, SkulkError>;
    /// Copy a local file to an absolute remote path.
    ///
    /// Used to ship `.skulk/.env` to an agent's worktree before the init hook runs.
    /// The remote path is interpolated into the transfer command without quoting, so
    /// it must be shell-safe (configuration values are validated in `config.rs`).
    fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<(), SkulkError>;
}
