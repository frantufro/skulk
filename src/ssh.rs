use crate::error::SkulkError;

pub(crate) trait Ssh {
    fn run(&self, cmd: &str) -> Result<String, SkulkError>;
    fn interactive(&self, cmd: &str) -> Result<std::process::ExitStatus, SkulkError>;
}
