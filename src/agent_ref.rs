use crate::config::Config;

/// A reference to an agent, pairing its user-facing `name` with the configured
/// session prefix so that the fully-qualified session/branch/worktree identifiers
/// can be produced without re-deriving them at every call site.
///
/// Construct from a bare `name` with [`AgentRef::new`], or from a prefix-qualified
/// string (tmux session name, branch name) with [`AgentRef::from_qualified`].
pub(crate) struct AgentRef {
    name: String,
    prefix: String,
}

impl AgentRef {
    /// Build an `AgentRef` from the user-facing agent name.
    pub fn new(name: &str, cfg: &Config) -> Self {
        Self {
            name: name.to_string(),
            prefix: cfg.session_prefix.clone(),
        }
    }

    /// Build an `AgentRef` from a prefix-qualified string (e.g. an entry from
    /// `AgentInventory.sessions`). If the prefix doesn't match, the entire
    /// string becomes the name — mirroring the old
    /// `strip_prefix(&**session_prefix).unwrap_or(s)` fallback used by `gc` and
    /// `destroy --all`.
    pub fn from_qualified(qualified: &str, cfg: &Config) -> Self {
        let name = qualified
            .strip_prefix(&*cfg.session_prefix)
            .unwrap_or(qualified)
            .to_string();
        Self {
            name,
            prefix: cfg.session_prefix.clone(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The fully-qualified identifier used as both the tmux session name and
    /// the git branch name.
    pub fn session_name(&self) -> String {
        format!("{}{}", self.prefix, self.name)
    }

    /// Same as [`Self::session_name`]; exposed under a branch-flavored alias
    /// to make intent obvious at call sites that are building git commands.
    pub fn branch_name(&self) -> String {
        self.session_name()
    }

    /// Absolute worktree path on the remote: `<worktree_base>/<session_name>`.
    pub fn worktree_path(&self, cfg: &Config) -> String {
        format!("{}/{}", cfg.worktree_base, self.session_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_config;

    #[test]
    fn new_exposes_name_verbatim() {
        let cfg = test_config();
        let agent = AgentRef::new("my-task", &cfg);
        assert_eq!(agent.name(), "my-task");
    }

    #[test]
    fn session_name_joins_prefix_and_name() {
        let cfg = test_config();
        let agent = AgentRef::new("my-task", &cfg);
        assert_eq!(agent.session_name(), "skulk-my-task");
    }

    #[test]
    fn branch_name_equals_session_name() {
        let cfg = test_config();
        let agent = AgentRef::new("my-task", &cfg);
        assert_eq!(agent.branch_name(), agent.session_name());
    }

    #[test]
    fn worktree_path_joins_base_and_session_name() {
        let cfg = test_config();
        let agent = AgentRef::new("my-task", &cfg);
        assert_eq!(
            agent.worktree_path(&cfg),
            "~/test-project-worktrees/skulk-my-task"
        );
    }

    #[test]
    fn from_qualified_strips_prefix() {
        let cfg = test_config();
        let agent = AgentRef::from_qualified("skulk-my-task", &cfg);
        assert_eq!(agent.name(), "my-task");
        assert_eq!(agent.session_name(), "skulk-my-task");
    }

    #[test]
    fn from_qualified_without_prefix_treats_whole_string_as_name() {
        // Preserves the `.strip_prefix(...).unwrap_or(s)` fallback so callers
        // that blindly iterate over upstream lists don't panic on unexpected
        // entries.
        let cfg = test_config();
        let agent = AgentRef::from_qualified("orphan", &cfg);
        assert_eq!(agent.name(), "orphan");
        assert_eq!(agent.session_name(), "skulk-orphan");
    }

    #[test]
    fn honors_configured_prefix() {
        let mut cfg = test_config();
        cfg.session_prefix = "bot-".into();
        let agent = AgentRef::new("task", &cfg);
        assert_eq!(agent.session_name(), "bot-task");
        assert_eq!(
            agent.worktree_path(&cfg),
            "~/test-project-worktrees/bot-task"
        );
    }
}
