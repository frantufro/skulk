use crate::agent_ref::AgentRef;
use crate::config::Config;
use crate::error::{SkulkError, classify_agent_error};
use crate::inventory::{AgentInventory, Lifecycle};
use crate::util::validate_name;

/// An agent the user named on the command line, resolved once at command
/// entry: the name is validated, the session prefix is applied, and the host
/// is remembered so that SSH failures can be reported in terms of this agent.
///
/// Handlers call [`Agent::resolve`] first and then use the accessors for
/// identifiers, [`Agent::classify`] for error mapping and
/// [`Agent::lifecycle`] for the existence check.
pub(crate) struct Agent {
    agent_ref: AgentRef,
    host: String,
}

impl Agent {
    /// Validate the user-typed `name` and qualify it with the configured
    /// session prefix.
    pub fn resolve(name: &str, cfg: &Config) -> Result<Self, SkulkError> {
        validate_name(name)?;
        Ok(Self {
            agent_ref: AgentRef::new(name, cfg),
            host: cfg.host.clone(),
        })
    }

    /// Resolve an agent from a prefix-qualified identifier taken from the
    /// inventory (tmux session, branch). The inventory only holds names that
    /// were validated when the agent was created.
    pub fn from_qualified(qualified: &str, cfg: &Config) -> Self {
        Self {
            agent_ref: AgentRef::from_qualified(qualified, cfg),
            host: cfg.host.clone(),
        }
    }

    pub fn name(&self) -> &str {
        self.agent_ref.name()
    }

    pub fn session_name(&self) -> String {
        self.agent_ref.session_name()
    }

    pub fn branch_name(&self) -> String {
        self.agent_ref.branch_name()
    }

    pub fn worktree_path(&self, cfg: &Config) -> String {
        self.agent_ref.worktree_path(cfg)
    }

    /// Where this agent stands in `inv`.
    pub fn lifecycle(&self, inv: &AgentInventory) -> Lifecycle {
        inv.lifecycle(&self.agent_ref)
    }

    /// Map a failure of an SSH call made on behalf of this agent to the
    /// user-facing error (see [`classify_agent_error`]).
    pub fn classify(&self, err: SkulkError) -> SkulkError {
        classify_agent_error(self.name(), err, &self.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_config;
    use std::collections::HashMap;

    #[test]
    fn resolve_rejects_invalid_name() {
        let cfg = test_config();
        let result = Agent::resolve("bad name", &cfg);
        assert!(matches!(result, Err(SkulkError::Validation(_))));
    }

    #[test]
    fn resolve_qualifies_name_with_prefix() {
        let cfg = test_config();
        let agent = Agent::resolve("my-task", &cfg).expect("valid name");
        assert_eq!(agent.name(), "my-task");
        assert_eq!(agent.session_name(), "skulk-my-task");
        assert_eq!(agent.branch_name(), "skulk-my-task");
    }

    #[test]
    fn from_qualified_strips_prefix() {
        let cfg = test_config();
        let agent = Agent::from_qualified("skulk-my-task", &cfg);
        assert_eq!(agent.name(), "my-task");
        assert_eq!(agent.session_name(), "skulk-my-task");
    }

    #[test]
    fn classify_reports_missing_session_as_not_found() {
        let cfg = test_config();
        let agent = Agent::resolve("foo", &cfg).expect("valid name");
        let err = agent.classify(SkulkError::SshFailed(
            "can't find session: skulk-foo".into(),
        ));
        assert!(matches!(err, SkulkError::NotFound(m) if m.contains("Agent 'foo' not found")));
    }

    #[test]
    fn lifecycle_reads_the_inventory() {
        let cfg = test_config();
        let agent = Agent::resolve("alpha", &cfg).expect("valid name");
        let mut inv = AgentInventory {
            sessions: vec![],
            worktrees: HashMap::new(),
            branches: vec![],
        };
        assert_eq!(agent.lifecycle(&inv), Lifecycle::Absent);

        inv.sessions.push("skulk-alpha".into());
        inv.worktrees
            .insert("skulk-alpha".into(), "/wt/skulk-alpha".into());
        assert_eq!(agent.lifecycle(&inv), Lifecycle::Running);
    }
}
