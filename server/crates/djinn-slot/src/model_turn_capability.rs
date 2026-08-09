//! Additive, redaction-safe B2 capability projection for a live slot route.

use djinn_provider::{
    ProviderAbortCapabilityV1, ProviderAttemptPlanV1, ProviderAttemptRouteCoverageV1,
    ProviderHiddenRetryCapabilityV1,
};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SlotLiveIdentity {
    pub pod_uid: String,
    pub deployment_revision: String,
}

impl SlotLiveIdentity {
    /// Uses the rendered downward-API UID and an operator/build revision only.
    pub fn from_environment() -> Option<Self> {
        let pod_uid = std::env::var("DJINN_TASK_RUN_POD_UID").ok()?;
        let pod_uid = pod_uid.trim();
        if pod_uid.is_empty() {
            return None;
        }
        // The Job renderer supplies the exact image reference it put on this
        // worker container (normally a digest-pinned pull ref). A package
        // version is deliberately not a fallback: rebuilt artifacts can retain
        // it and would collapse distinct live deployment revisions in B2.
        let deployment_revision = std::env::var("DJINN_DEPLOYMENT_REVISION").ok()?;
        let deployment_revision = deployment_revision.trim();
        if deployment_revision.is_empty() {
            return None;
        }
        Some(Self {
            pod_uid: pod_uid.to_owned(),
            deployment_revision: deployment_revision.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnCapabilityCoverageV2 {
    Covered,
    Uncovered,
}

/// This carries no credential, account/project, user, request, or lease ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelTurnCapabilityReportV2 {
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider: String,
    pub model_scope: String,
    pub coverage: ModelTurnCapabilityCoverageV2,
}

/// Additive Phase C consumer seam; slot code emits but never aggregates reports.
pub trait ModelTurnCapabilityReporter: Send + Sync {
    fn emit(&self, report: &ModelTurnCapabilityReportV2);
}

/// A positive B2 report requires the exact B1 route and both B1 capabilities.
pub fn report_for_route(
    identity: &SlotLiveIdentity,
    provider: &str,
    model_scope: &str,
    plan: Option<&ProviderAttemptPlanV1>,
) -> ModelTurnCapabilityReportV2 {
    let route_is_covered = plan
        .filter(|plan| plan.scope.provider_id == provider && plan.scope.model_id == model_scope)
        .is_some_and(|plan| {
            matches!(plan.coverage,
            ProviderAttemptRouteCoverageV1::Covered { capabilities, .. }
                if capabilities.hidden_retries == ProviderHiddenRetryCapabilityV1::Disabled
                    && capabilities.abort == ProviderAbortCapabilityV1::Supported)
        });
    let coverage = if route_is_covered {
        ModelTurnCapabilityCoverageV2::Covered
    } else {
        ModelTurnCapabilityCoverageV2::Uncovered
    };
    ModelTurnCapabilityReportV2 {
        slot_pod_uid: identity.pod_uid.clone(),
        deployment_revision: identity.deployment_revision.clone(),
        provider: provider.to_owned(),
        model_scope: model_scope.to_owned(),
        coverage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::{ModelTurnBucketDebit, ModelTurnBucketKind};
    use djinn_provider::{
        ProviderAdmissionPolicyV1, ProviderAttemptAbortHandleV1, ProviderAttemptCapabilitiesV1,
        ProviderAttemptScopeV1, ProviderCredentialRecordScopeV1, ProviderOutputReservationSourceV1,
    };

    fn plan(
        provider: &str,
        model: &str,
        coverage: ProviderAttemptRouteCoverageV1,
    ) -> ProviderAttemptPlanV1 {
        ProviderAttemptPlanV1 {
            scope: ProviderAttemptScopeV1 {
                credential: ProviderCredentialRecordScopeV1::from_credential_record_id(
                    "credential-secret",
                ),
                provider_id: provider.into(),
                model_id: model.into(),
            },
            coverage,
            debits: vec![ModelTurnBucketDebit {
                bucket_kind: ModelTurnBucketKind::Request,
                units: 1,
            }],
            output_reservation_source: ProviderOutputReservationSourceV1::ExplicitLimit,
            abort: ProviderAttemptAbortHandleV1::new(),
        }
    }
    fn covered() -> ProviderAttemptRouteCoverageV1 {
        ProviderAttemptRouteCoverageV1::Covered {
            capabilities: ProviderAttemptCapabilitiesV1 {
                hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
                abort: ProviderAbortCapabilityV1::Supported,
            },
            supported_bucket_bindings: vec![ModelTurnBucketKind::Request],
            policy: ProviderAdmissionPolicyV1::Proactive,
        }
    }
    #[test]
    fn reports_only_exact_supported_route_without_sensitive_ids() {
        let identity = SlotLiveIdentity {
            pod_uid: "pod-a".into(),
            deployment_revision: "rev-a".into(),
        };
        let report = report_for_route(
            &identity,
            "openai",
            "gpt",
            Some(&plan("openai", "gpt", covered())),
        );
        assert_eq!(report.coverage, ModelTurnCapabilityCoverageV2::Covered);
        let serialized = serde_json::to_string(&report).expect("serialize report");
        for forbidden in [
            "credential-secret",
            "account",
            "project",
            "user",
            "request",
            "lease",
        ] {
            assert!(!serialized.contains(forbidden), "report leaked {forbidden}");
        }
    }
    #[test]
    fn rejects_incomplete_or_mismatched_routes_and_changes_identity_keys() {
        let first = SlotLiveIdentity {
            pod_uid: "pod-a".into(),
            deployment_revision: "rev-a".into(),
        };
        let second = SlotLiveIdentity {
            pod_uid: "pod-b".into(),
            deployment_revision: "rev-b".into(),
        };
        let incomplete = ProviderAttemptRouteCoverageV1::Covered {
            capabilities: ProviderAttemptCapabilitiesV1 {
                hidden_retries: ProviderHiddenRetryCapabilityV1::Unsupported,
                abort: ProviderAbortCapabilityV1::Supported,
            },
            supported_bucket_bindings: vec![],
            policy: ProviderAdmissionPolicyV1::Proactive,
        };
        assert_eq!(
            report_for_route(
                &first,
                "openai",
                "gpt",
                Some(&plan("openai", "gpt", incomplete))
            )
            .coverage,
            ModelTurnCapabilityCoverageV2::Uncovered
        );
        assert_eq!(
            report_for_route(
                &first,
                "openai",
                "gpt",
                Some(&plan("other", "gpt", covered()))
            )
            .coverage,
            ModelTurnCapabilityCoverageV2::Uncovered
        );
        assert_ne!(
            report_for_route(&first, "openai", "gpt", None),
            report_for_route(&second, "openai", "gpt", None)
        );
    }
}
