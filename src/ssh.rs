use std::path::Path;
use std::time::Duration;

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

    /// Copy a remote file to a local path.
    ///
    /// Used to retrieve Claude Code session files from a remote agent's
    /// `~/.claude/projects/` directory.
    fn download_file(&self, remote_path: &str, local_path: &Path) -> Result<(), SkulkError>;

    /// Start a long-running remote command and read its stdout line by line.
    ///
    /// Used by `skulk events`, which holds one connection open for its whole
    /// lifetime. Dropping the stream ends the command.
    fn stream(&self, cmd: &str) -> Result<Box<dyn LineStream>, SkulkError>;
}

/// One read from a [`LineStream`].
#[derive(Debug)]
pub(crate) enum StreamRecv {
    /// A line of stdout, without its trailing newline.
    Line(String),
    /// Nothing arrived within the timeout.
    Timeout,
    /// Stdout reached EOF or the command exited; carries the reason.
    Closed(SkulkError),
}

/// Stdout of a command started with [`Ssh::stream`].
pub(crate) trait LineStream {
    /// Wait up to `timeout` for the next line.
    fn recv(&mut self, timeout: Duration) -> StreamRecv;
}
