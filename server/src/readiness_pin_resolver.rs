//! Server composition-root implementation of readiness native-pin resolution.
//!
//! The control-plane service depends only on its resolver trait; this module is
//! the one place that couples that trait to the agent-owned native registry.

use async_trait::async_trait;
use djinn_agent::native_skills::{NativeSkillLookupError, native_skill_exact};
use djinn_control_plane::readiness_kickoff::{ReadinessSkillPinError, ReadinessSkillPinResolver};

#[derive(Clone, Copy, Debug, Default)]
pub struct AgentNativeReadinessPinResolver;

#[async_trait]
impl ReadinessSkillPinResolver for AgentNativeReadinessPinResolver {
    async fn resolve_exact(
        &self,
        name: &'static str,
        version: &'static str,
    ) -> Result<(), ReadinessSkillPinError> {
        native_skill_exact(name, version)
            .map(|_| ())
            .map_err(|error| match error {
                NativeSkillLookupError::UnknownName { name } => {
                    ReadinessSkillPinError::Unavailable {
                        detail: format!("native skill {name} is not registered"),
                    }
                }
                NativeSkillLookupError::VersionMismatch {
                    name,
                    registered_version,
                    ..
                } => ReadinessSkillPinError::WrongPin {
                    registered_name: name,
                    registered_version,
                },
            })
    }
}
