//! Task-local session context used to thread the authenticated user's
//! identity (both their GitHub access token and the stable `users.id`
//! surrogate) down into MCP tool handlers and repository inserts.
//!
//! The HTTP MCP handler authenticates the incoming request, resolves the
//! caller's `github_access_token` and `users.id`, and scopes the dispatch
//! under both [`SESSION_USER_TOKEN`] and [`SESSION_USER_ID`]. Deep in the
//! stack, tools that need to call the GitHub user API (e.g.
//! `github_app_installations`, `github_list_repos`) read the token via
//! [`current_user_token`]; repositories that need to stamp
//! `created_by_user_id` on new rows read the id via [`current_user_id`].
//! Neither require plumbing through every function signature.
//!
//! When no session is present (unauthenticated request, or any internal
//! caller that did not set up the scope), the accessors return `None` and
//! the consumer either returns a clear "sign in"-style error (for token
//! consumers) or simply leaves the `created_by_user_id` column as NULL
//! (for repository consumers — agent-spawned rows without a user context
//! are allowed to be unattributed).

tokio::task_local! {
    pub static SESSION_USER_TOKEN: Option<String>;
    /// Stable `users.id` surrogate (VARCHAR(36)) for the authenticated
    /// session, threaded through MCP dispatch so repository inserts can
    /// stamp `created_by_user_id` without plumbing the id through every
    /// call site.
    pub static SESSION_USER_ID: Option<String>;
    /// Trusted revision caller set only by authenticated HTTP dispatch or a
    /// server-owned agent execution scope; tool arguments never populate it.
    pub static REVISION_CALLER_CONTEXT: Option<TrustedRevisionCallerContext>;
}

/// A server-authenticated principal allowed to cause a note revision.
/// There is deliberately no anonymous or system variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustedRevisionPrincipal {
    Human { user_id: String },
    Agent { agent_id: String },
}

/// Server-owned caller plus optional active execution provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedRevisionCallerContext {
    principal: TrustedRevisionPrincipal,
    session_id: Option<String>,
    task_id: Option<String>,
    task_run_id: Option<String>,
}

impl TrustedRevisionCallerContext {
    pub fn authenticated_human(user_id: impl Into<String>) -> Option<Self> {
        Self::new(TrustedRevisionPrincipal::Human {
            user_id: user_id.into(),
        })
    }

    pub fn authenticated_agent(agent_id: impl Into<String>) -> Option<Self> {
        Self::new(TrustedRevisionPrincipal::Agent {
            agent_id: agent_id.into(),
        })
    }

    fn new(principal: TrustedRevisionPrincipal) -> Option<Self> {
        let id = match &principal {
            TrustedRevisionPrincipal::Human { user_id } => user_id,
            TrustedRevisionPrincipal::Agent { agent_id } => agent_id,
        };
        (!id.trim().is_empty()).then_some(Self {
            principal,
            session_id: None,
            task_id: None,
            task_run_id: None,
        })
    }

    /// Attach IDs obtained from active server execution, never tool input.
    pub fn with_execution_provenance(
        mut self,
        session_id: Option<String>,
        task_id: Option<String>,
        task_run_id: Option<String>,
    ) -> Self {
        self.session_id = non_blank(session_id);
        self.task_id = non_blank(task_id);
        self.task_run_id = non_blank(task_run_id);
        self
    }

    pub fn principal(&self) -> &TrustedRevisionPrincipal {
        &self.principal
    }
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }
    pub fn task_run_id(&self) -> Option<&str> {
        self.task_run_id.as_deref()
    }
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// Read the current task-local session user token, if any.
///
/// Returns `None` when:
/// - the request was unauthenticated,
/// - the task was spawned outside any [`SESSION_USER_TOKEN`] scope.
pub fn current_user_token() -> Option<String> {
    SESSION_USER_TOKEN.try_with(|t| t.clone()).ok().flatten()
}

/// Read the current task-local session `users.id`, if any.
///
/// Returns `None` when:
/// - the request was unauthenticated (no session cookie),
/// - the task was spawned outside any [`SESSION_USER_ID`] scope (e.g. the
///   agent coordinator's internal loops, background workers).
///
/// Repository inserts consult this to populate the `created_by_user_id`
/// attribution column on `tasks`, `epics`, and `sessions`.
pub fn current_user_id() -> Option<String> {
    SESSION_USER_ID.try_with(|t| t.clone()).ok().flatten()
}

/// Read the authenticated revision caller scoped for this dispatch.
pub fn current_revision_caller_context() -> Option<TrustedRevisionCallerContext> {
    REVISION_CALLER_CONTEXT
        .try_with(|context| context.clone())
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_context_has_null_provenance_without_execution() {
        let context = TrustedRevisionCallerContext::authenticated_human("user-1").unwrap();
        assert!(
            matches!(context.principal(), TrustedRevisionPrincipal::Human { user_id } if user_id == "user-1")
        );
        assert_eq!(
            (
                context.session_id(),
                context.task_id(),
                context.task_run_id()
            ),
            (None, None, None)
        );
    }

    #[test]
    fn agent_context_preserves_active_execution_provenance() {
        let context = TrustedRevisionCallerContext::authenticated_agent("worker")
            .unwrap()
            .with_execution_provenance(
                Some("session".into()),
                Some("task".into()),
                Some("run".into()),
            );
        assert!(
            matches!(context.principal(), TrustedRevisionPrincipal::Agent { agent_id } if agent_id == "worker")
        );
        assert_eq!(
            (
                context.session_id(),
                context.task_id(),
                context.task_run_id()
            ),
            (Some("session"), Some("task"), Some("run"))
        );
    }

    #[tokio::test]
    async fn revision_context_is_task_local() {
        let human = TrustedRevisionCallerContext::authenticated_human("human");
        let agent = TrustedRevisionCallerContext::authenticated_agent("agent");
        let observed = REVISION_CALLER_CONTEXT
            .scope(human, async {
                let outer = current_revision_caller_context();
                let nested = REVISION_CALLER_CONTEXT
                    .scope(agent, async { current_revision_caller_context() })
                    .await;
                (outer, nested, current_revision_caller_context())
            })
            .await;
        assert!(matches!(
            observed.0.unwrap().principal(),
            TrustedRevisionPrincipal::Human { .. }
        ));
        assert!(matches!(
            observed.1.unwrap().principal(),
            TrustedRevisionPrincipal::Agent { .. }
        ));
        assert!(matches!(
            observed.2.unwrap().principal(),
            TrustedRevisionPrincipal::Human { .. }
        ));
        assert!(current_revision_caller_context().is_none());
    }
}
