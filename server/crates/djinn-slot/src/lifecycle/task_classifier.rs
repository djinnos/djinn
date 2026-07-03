//! Canonical task-based classifier for native-skill loading triggers.

use djinn_core::models::Task;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSkillTrigger {
    ProposalAuthoring,
}

pub fn classify_native_skill_trigger_by_type(
    role_name: &str,
    issue_type: &str,
) -> Option<NativeSkillTrigger> {
    match (role_name, issue_type) {
        ("planner", "epic_breakdown") | ("advocate", "refinement") => {
            Some(NativeSkillTrigger::ProposalAuthoring)
        }
        _ => None,
    }
}

pub fn classify_native_skill_trigger(role_name: &str, task: &Task) -> Option<NativeSkillTrigger> {
    classify_native_skill_trigger_by_type(role_name, task.issue_type.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proposal_authoring_roles_trigger_native_skills() {
        assert_eq!(
            classify_native_skill_trigger_by_type("planner", "epic_breakdown"),
            Some(NativeSkillTrigger::ProposalAuthoring)
        );
        assert_eq!(
            classify_native_skill_trigger_by_type("advocate", "refinement"),
            Some(NativeSkillTrigger::ProposalAuthoring)
        );
    }
    #[test]
    fn non_authoring_roles_and_issue_types_do_not_trigger_native_skills() {
        for (role, issue_type) in [
            ("planner", "planning"),
            ("planner", "decomposition"),
            ("planner", "review"),
            ("worker", "epic_breakdown"),
            ("reviewer", "epic_breakdown"),
            ("adversary", "refinement"),
            ("judge", "refinement"),
            ("", "epic_breakdown"),
            ("planner", ""),
        ] {
            assert_eq!(
                classify_native_skill_trigger_by_type(role, issue_type),
                None,
                "{role}/{issue_type} should not load authoring native skills"
            );
        }
    }
}
