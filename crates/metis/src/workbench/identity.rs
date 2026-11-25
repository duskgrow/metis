//! Typed logical identities; display labels and visible rows never participate.

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct ProjectKey {
    pub(super) host_id: String,
    pub(super) project_id: String,
}

impl ProjectKey {
    #[cfg(test)]
    pub(super) fn new(host_id: impl Into<String>, project_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            project_id: project_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct WorktreeKey {
    pub(super) project: ProjectKey,
    pub(super) worktree_id: String,
}

impl WorktreeKey {
    #[cfg(test)]
    pub(super) fn new(project: ProjectKey, worktree_id: impl Into<String>) -> Self {
        Self {
            project,
            worktree_id: worktree_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct SessionKey {
    pub(super) worktree: WorktreeKey,
    pub(super) session_id: String,
}

impl SessionKey {
    #[cfg(test)]
    pub(super) fn new(worktree: WorktreeKey, session_id: impl Into<String>) -> Self {
        Self {
            worktree,
            session_id: session_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct SubagentKey {
    pub(super) parent: SessionKey,
    pub(super) subagent_id: String,
}

impl SubagentKey {
    #[cfg(test)]
    pub(super) fn new(parent: SessionKey, subagent_id: impl Into<String>) -> Self {
        Self {
            parent,
            subagent_id: subagent_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct ToolCallKey {
    pub(super) session: SessionKey,
    pub(super) call_id: String,
}

impl ToolCallKey {
    #[cfg(test)]
    pub(super) fn new(session: SessionKey, call_id: impl Into<String>) -> Self {
        Self {
            session,
            call_id: call_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(host: &str, id: &str) -> ProjectKey {
        ProjectKey::new(host, id)
    }

    fn worktree(host: &str, project_id: &str, id: &str) -> WorktreeKey {
        WorktreeKey::new(project(host, project_id), id)
    }

    fn session(host: &str, project_id: &str, worktree_id: &str, id: &str) -> SessionKey {
        SessionKey::new(worktree(host, project_id, worktree_id), id)
    }

    #[test]
    fn project_and_worktree_identity_include_every_owner() {
        assert_eq!(project("host-a", "project"), project("host-a", "project"));
        assert_ne!(project("host-a", "project"), project("host-b", "project"));
        assert_ne!(
            worktree("host-a", "project-a", "worktree"),
            worktree("host-a", "project-b", "worktree")
        );
        assert_ne!(
            worktree("host-a", "project", "worktree-a"),
            worktree("host-a", "project", "worktree-b")
        );
    }

    #[test]
    fn session_identity_retains_its_worktree() {
        let first = session("host", "project", "worktree-a", "session");
        let same = first.clone();
        let shared_id_elsewhere = session("host", "project", "worktree-b", "session");

        assert_eq!(first, same);
        assert_ne!(first, shared_id_elsewhere);
        assert_eq!(first.worktree, worktree("host", "project", "worktree-a"));
    }

    #[test]
    fn subagent_and_tool_call_identity_include_their_parents() {
        let parent_a = session("host", "project", "worktree", "session-a");
        let parent_b = session("host", "project", "worktree", "session-b");

        assert_ne!(
            SubagentKey::new(parent_a.clone(), "child"),
            SubagentKey::new(parent_b.clone(), "child")
        );
        assert_ne!(
            ToolCallKey::new(parent_a, "call"),
            ToolCallKey::new(parent_b, "call")
        );
    }
}
