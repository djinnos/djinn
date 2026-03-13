mod conflict;
mod groomer;
mod pm;
mod reviewer;
mod worker;

pub(super) use conflict::CONFLICT_RESOLVER_CONFIG;
pub(super) use groomer::GROOMER_CONFIG;
pub(super) use pm::PM_CONFIG;
pub(super) use reviewer::TASK_REVIEWER_CONFIG;
pub(super) use worker::WORKER_CONFIG;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent::prompts::{self, TaskContext};
use crate::agent::AgentType;
use crate::models::{Task, TransitionAction};

#[derive(Debug, Clone)]
pub(super) struct CompactionPrompts {
    pub mid_session: &'static str,
    pub mid_session_system: &'static str,
    pub pre_resume: &'static str,
    pub pre_resume_system: &'static str,
}

#[derive(Debug, Clone)]
pub(super) struct RoleConfig {
    pub name: &'static str,
    pub dispatch_role: &'static str,
    pub tool_schemas: fn() -> Vec<Value>,
    pub initial_message: &'static str,
    pub compaction: CompactionPrompts,
    pub preserves_session: bool,
    pub is_project_scoped: bool,
}

pub struct DispatchContext {
    pub has_conflict_context: bool,
}

pub struct DispatchRule {
    pub role_name: &'static str,
    pub claims: fn(&Task, &DispatchContext) -> bool,
    pub start_action: fn(&str) -> Option<TransitionAction>,
    pub release_action: TransitionAction,
}

pub struct RoleRegistry {
    pub roles: HashMap<&'static str, AgentType>,
    pub dispatch_rules: Vec<DispatchRule>,
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    None,
    Start,
    Release,
}

pub struct AgentContext<'a> {
    pub task: &'a Task,
    pub task_ctx: &'a TaskContext,
    pub project_dir: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOutcome {
    pub transition: Transition,
}

impl AgentOutcome {
    pub fn none() -> Self {
        Self {
            transition: Transition::None,
        }
    }
}

pub trait AgentRole {
    fn config(&self) -> RoleConfig;
    fn render_prompt(&self, ctx: &AgentContext<'_>) -> String;
    fn on_complete(&self, _ctx: &AgentContext<'_>) -> AgentOutcome {
        AgentOutcome::none()
    }
    fn prepare_worktree(&self, ctx: &AgentContext<'_>) -> PathBuf {
        ctx.project_dir.join(".djinn").join("worktrees").join(&ctx.task.short_id)
    }
}

pub struct WorkerRole;
pub struct TaskReviewerRole;
pub struct PmRole;
pub struct GroomerRole;
pub struct ConflictResolverRole;

impl AgentRole for WorkerRole {
    fn config(&self) -> RoleConfig {
        WORKER_CONFIG
    }

    fn render_prompt(&self, ctx: &AgentContext<'_>) -> String {
        prompts::render_prompt(AgentType::Worker, ctx.task, ctx.task_ctx)
    }
}

impl AgentRole for TaskReviewerRole {
    fn config(&self) -> RoleConfig {
        TASK_REVIEWER_CONFIG
    }

    fn render_prompt(&self, ctx: &AgentContext<'_>) -> String {
        prompts::render_prompt(AgentType::TaskReviewer, ctx.task, ctx.task_ctx)
    }
}

impl AgentRole for PmRole {
    fn config(&self) -> RoleConfig {
        PM_CONFIG
    }

    fn render_prompt(&self, ctx: &AgentContext<'_>) -> String {
        prompts::render_prompt(AgentType::PM, ctx.task, ctx.task_ctx)
    }
}

impl AgentRole for GroomerRole {
    fn config(&self) -> RoleConfig {
        GROOMER_CONFIG
    }

    fn render_prompt(&self, ctx: &AgentContext<'_>) -> String {
        prompts::render_prompt(AgentType::Groomer, ctx.task, ctx.task_ctx)
    }
}

impl AgentRole for ConflictResolverRole {
    fn config(&self) -> RoleConfig {
        CONFLICT_RESOLVER_CONFIG
    }

    fn render_prompt(&self, ctx: &AgentContext<'_>) -> String {
        prompts::render_prompt(AgentType::ConflictResolver, ctx.task, ctx.task_ctx)
    }

    fn prepare_worktree(&self, ctx: &AgentContext<'_>) -> PathBuf {
        ctx.project_dir.join(".djinn").join("worktrees").join(&ctx.task.short_id)
    }
}

impl RoleRegistry {
    pub fn new() -> Self {
        let roles = HashMap::from([
            ("worker", AgentType::Worker),
            ("conflict_resolver", AgentType::ConflictResolver),
            ("task_reviewer", AgentType::TaskReviewer),
            ("pm", AgentType::PM),
            ("groomer", AgentType::Groomer),
        ]);

        let dispatch_rules = vec![
            conflict_resolver_dispatch_rule(),
            worker_dispatch_rule(),
            task_reviewer_dispatch_rule(),
            pm_dispatch_rule(),
            groomer_dispatch_rule(),
        ];

        Self {
            roles,
            dispatch_rules,
        }
    }

    pub fn role_for_task(&self, task: &Task, ctx: &DispatchContext) -> Option<&'static str> {
        self.dispatch_rules
            .iter()
            .find(|rule| (rule.claims)(task, ctx))
            .map(|rule| rule.role_name)
    }

    pub fn dispatch_roles(&self) -> Vec<&'static str> {
        self.dispatch_rules
            .iter()
            .map(|r| r.role_name)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }
}

fn worker_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    !matches!(
        task.status.as_str(),
        "needs_task_review" | "in_task_review" | "needs_pm_intervention" | "in_pm_intervention"
    )
}

fn worker_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::Worker.start_action(status)
}

fn worker_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "worker",
        claims: worker_claims,
        start_action: worker_start_action,
        release_action: AgentType::Worker.release_action(),
    }
}

fn conflict_resolver_claims(task: &Task, ctx: &DispatchContext) -> bool {
    task.status == "open" && ctx.has_conflict_context
}

fn conflict_resolver_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::ConflictResolver.start_action(status)
}

fn conflict_resolver_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "conflict_resolver",
        claims: conflict_resolver_claims,
        start_action: conflict_resolver_start_action,
        release_action: AgentType::ConflictResolver.release_action(),
    }
}

fn task_reviewer_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(task.status.as_str(), "needs_task_review" | "in_task_review")
}

fn task_reviewer_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::TaskReviewer.start_action(status)
}

fn task_reviewer_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "task_reviewer",
        claims: task_reviewer_claims,
        start_action: task_reviewer_start_action,
        release_action: AgentType::TaskReviewer.release_action(),
    }
}

fn pm_claims(task: &Task, _ctx: &DispatchContext) -> bool {
    matches!(
        task.status.as_str(),
        "needs_pm_intervention" | "in_pm_intervention"
    )
}

fn pm_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::PM.start_action(status)
}

fn pm_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "pm",
        claims: pm_claims,
        start_action: pm_start_action,
        release_action: AgentType::PM.release_action(),
    }
}

fn groomer_claims(_task: &Task, _ctx: &DispatchContext) -> bool {
    false
}

fn groomer_start_action(status: &str) -> Option<TransitionAction> {
    AgentType::Groomer.start_action(status)
}

fn groomer_dispatch_rule() -> DispatchRule {
    DispatchRule {
        role_name: "groomer",
        claims: groomer_claims,
        start_action: groomer_start_action,
        release_action: AgentType::Groomer.release_action(),
    }
}
<<<<<<< HEAD
=======

#[cfg(test)]
mod tests {
    use super::{
        AgentContext, AgentRole, ConflictResolverRole, DispatchContext, GroomerRole, PmRole,
        RoleRegistry, TaskReviewerRole, WorkerRole,
    };
    use crate::agent::AgentType;
    use crate::models::{Task, TransitionAction};

    fn task_with_status(status: &str) -> Task {
        Task {
            id: "t1".to_string(),
            title: "t".to_string(),
            description: "d".to_string(),
            status: status.to_string(),
            priority: 1,
            issue_type: "task".to_string(),
            parent_id: None,
            acceptance_criteria: "[]".to_string(),
            notes: None,
            assignee: None,
            external_ref: None,
            created_at: "".to_string(),
            updated_at: "".to_string(),
            project_id: "p1".to_string(),
        }
    }

    #[test]
    fn role_for_task_matches_legacy_for_all_status_conflict_combos() {
        let registry = RoleRegistry::new();
        let statuses = [
            "backlog",
            "open",
            "in_progress",
            "verifying",
            "needs_task_review",
            "in_task_review",
            "needs_pm_intervention",
            "in_pm_intervention",
            "closed",
        ];

        for status in statuses {
            for has_conflict_context in [false, true] {
                let task = task_with_status(status);
                let ctx = DispatchContext {
                    has_conflict_context,
                };
                let expected = AgentType::for_task_status(status, has_conflict_context).as_str();
                let actual = registry
                    .role_for_task(&task, &ctx)
                    .expect("every status should map to some role");
                assert_eq!(actual, expected, "status={status} conflict={has_conflict_context}");
            }
        }
    }

    #[test]
    fn conflict_resolver_priority_over_worker_for_open_with_conflict() {
        let registry = RoleRegistry::new();
        let task = task_with_status("open");

        let with_conflict = registry.role_for_task(
            &task,
            &DispatchContext {
                has_conflict_context: true,
            },
        );
        assert_eq!(with_conflict, Some("conflict_resolver"));

        let without_conflict = registry.role_for_task(
            &task,
            &DispatchContext {
                has_conflict_context: false,
            },
        );
        assert_eq!(without_conflict, Some("worker"));
    }

    #[test]
    fn dispatch_rule_actions_match_agent_type_methods() {
        let registry = RoleRegistry::new();

        for rule in &registry.dispatch_rules {
            let agent = registry
                .roles
                .get(rule.role_name)
                .expect("rule role should be registered");

            for status in [
                "backlog",
                "open",
                "in_progress",
                "verifying",
                "needs_task_review",
                "in_task_review",
                "needs_pm_intervention",
                "in_pm_intervention",
                "closed",
            ] {
                assert_eq!((rule.start_action)(status), agent.start_action(status));
            }

            assert_eq!(rule.release_action, agent.release_action());
        }
    }

    #[test]
    fn dispatch_roles_returns_all_distinct_dispatch_roles() {
        let registry = RoleRegistry::new();
        let mut roles = registry.dispatch_roles();
        roles.sort_unstable();
        assert_eq!(
            roles,
            vec!["conflict_resolver", "groomer", "pm", "task_reviewer", "worker"]
        );
    }

    #[test]
    fn role_for_specific_statuses() {
        let registry = RoleRegistry::new();

        assert_eq!(
            registry.role_for_task(
                &task_with_status("open"),
                &DispatchContext {
                    has_conflict_context: false,
                }
            ),
            Some("worker")
        );
        assert_eq!(
            registry.role_for_task(
                &task_with_status("needs_task_review"),
                &DispatchContext {
                    has_conflict_context: false,
                }
            ),
            Some("task_reviewer")
        );
        assert_eq!(
            registry.role_for_task(
                &task_with_status("in_task_review"),
                &DispatchContext {
                    has_conflict_context: false,
                }
            ),
            Some("task_reviewer")
        );
        assert_eq!(
            registry.role_for_task(
                &task_with_status("needs_pm_intervention"),
                &DispatchContext {
                    has_conflict_context: false,
                }
            ),
            Some("pm")
        );
        assert_eq!(
            registry.role_for_task(
                &task_with_status("in_pm_intervention"),
                &DispatchContext {
                    has_conflict_context: false,
                }
            ),
            Some("pm")
        );
        assert_eq!(AgentType::Worker.release_action(), TransitionAction::Release);
    }
}


    fn sample_task_context() -> crate::agent::prompts::TaskContext {
        crate::agent::prompts::TaskContext {
            project_path: "/tmp/project".to_string(),
            workspace_path: "/tmp/project/.djinn/worktrees/t1".to_string(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files: None,
            merge_base_branch: None,
            merge_target_branch: None,
            merge_failure_context: None,
            setup_commands: None,
            verification_commands: None,
            activity: None,
        }
    }

    #[test]
    fn role_render_prompt_equivalence() {
        let task = task_with_status("open");
        let task_ctx = sample_task_context();
        let role_ctx = AgentContext {
            task: &task,
            task_ctx: &task_ctx,
            project_dir: std::path::Path::new("/tmp/project"),
        };

        let worker = WorkerRole;
        assert_eq!(
            worker.render_prompt(&role_ctx),
            crate::agent::prompts::render_prompt(AgentType::Worker, &task, &task_ctx)
        );

        let reviewer = TaskReviewerRole;
        assert_eq!(
            reviewer.render_prompt(&role_ctx),
            crate::agent::prompts::render_prompt(AgentType::TaskReviewer, &task, &task_ctx)
        );

        let pm = PmRole;
        assert_eq!(
            pm.render_prompt(&role_ctx),
            crate::agent::prompts::render_prompt(AgentType::PM, &task, &task_ctx)
        );

        let groomer = GroomerRole;
        assert_eq!(
            groomer.render_prompt(&role_ctx),
            crate::agent::prompts::render_prompt(AgentType::Groomer, &task, &task_ctx)
        );

        let conflict = ConflictResolverRole;
        assert_eq!(
            conflict.render_prompt(&role_ctx),
            crate::agent::prompts::render_prompt(AgentType::ConflictResolver, &task, &task_ctx)
        );
    }
>>>>>>> origin/main
