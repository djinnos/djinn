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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_control_plane::readiness_kickoff::{READINESS_SKILL_NAME, READINESS_SKILL_VERSION};

    /// The protocol pin the control plane demands and the version the agent
    /// registry actually registers are separate constants in separate crates.
    /// If they drift, nothing fails to compile — every readiness kickoff just
    /// starts returning `WrongPin` at runtime. Bumping the catalog therefore
    /// has to move both, and this is what proves it did.
    #[tokio::test]
    async fn control_plane_pin_resolves_against_the_registered_native_catalog() {
        AgentNativeReadinessPinResolver
            .resolve_exact(READINESS_SKILL_NAME, READINESS_SKILL_VERSION)
            .await
            .expect("the readiness protocol pin must resolve against the native registry");
    }

    /// `ui/src/pages/fixtures/readiness_terminal_detail.json` is a shared wire
    /// contract, not a recorded historical run: the routed Axum regression
    /// imports it with `include_str!` and compares it field-by-field against a
    /// live serialized detail response, which stamps `READINESS_SKILL_VERSION`.
    /// Leaving it behind on a catalog bump therefore breaks that regression.
    /// This is the cheap, database-free guard that names the drift directly.
    #[test]
    fn shared_browser_detail_fixture_tracks_the_readiness_pin() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../ui/src/pages/fixtures/readiness_terminal_detail.json"
        ))
        .expect("valid shared browser detail fixture");
        assert_eq!(
            fixture["run"]["skill_name"], READINESS_SKILL_NAME,
            "the shared browser fixture must name the pinned readiness skill"
        );
        assert_eq!(
            fixture["run"]["skill_version"], READINESS_SKILL_VERSION,
            "bumping the readiness catalog must also move the shared browser \
             detail fixture; it is a wire contract the routed regression \
             compares against a live response, not a historical record"
        );
    }
}
