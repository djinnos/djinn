//! Startup verification that the namespace djinn-server runs in is actually
//! Kueue-managed before anything is allowed to render an armed Job.
//!
//! # Why this exists
//!
//! `kueue.armed` drives two halves of one contract: the
//! `djinn.io/kueue-managed` label on the Namespace, and `DJINN_KUEUE_ARMED` on
//! djinn-server, which makes the task-run, warm and standalone-SCIP renderers
//! emit `suspend: true` plus a `kueue.x-k8s.io/queue-name` label. Kueue captures
//! Jobs ONLY in a labelled namespace, so an armed Job in an unlabelled namespace
//! is never captured, never unsuspended, and hangs forever — every build, until
//! someone notices.
//!
//! The chart keeps the halves together, but it cannot label a namespace it does
//! not render: with `namespace.create=false` there is no Namespace object.
//! `deploy/helm/djinn/templates/namespace.yaml` refuses that combination unless
//! the operator sets `kueue.namespaceLabelledExternally=true`. That value is an
//! unverified CLAIM — Helm cannot see cluster state, so an operator who sets it
//! and is wrong reaches the identical hang.
//!
//! This module closes that hole with the one check Helm cannot do: at startup
//! the server GETs its own Namespace and reads the label back off the live
//! object.
//!
//! # Arm only on positive proof
//!
//! [`decide`] arms only when the label is confirmed present. Every other outcome
//! — label absent, label present with an unexpected value, RBAC forbidden,
//! apiserver unreachable — disarms. That asymmetry is deliberate:
//!
//! * disarmed-when-it-should-be-armed costs quota enforcement; Jobs still render
//!   (unsuspended) and still run. It is the pre-cutover status quo.
//! * armed-when-it-should-be-disarmed hangs every build Job forever.
//!
//! The failure modes are not symmetric, so the tie-break is not either.
//!
//! Refusal disarms rather than aborting the process for the same reason: a
//! crash-looping server is a total outage, which is no better than the hang it
//! was trying to prevent.
//!
//! # RBAC
//!
//! Namespaces are cluster-scoped, but the apiserver derives the request's
//! namespace attribute from the object name for `GET
//! /api/v1/namespaces/<name>`, so a NAMESPACED `Role` granting `get` on
//! `namespaces` authorizes a ServiceAccount to read its own namespace and
//! nothing else. Verified against a real apiserver (kind, k8s v1.34): with only
//! that Role, GET of the own namespace returns 200 while GET of another
//! namespace and LIST namespaces both return 403.
//!
//! That is what `deploy/helm/djinn/templates/role-controller.yaml` grants, which
//! is why this check does not need — and must not acquire — the cluster-wide
//! permission that `role-tokenreview.yaml` documents as the server's only one.
//!
//! # Re-proving the live half
//!
//! [`decide`] and [`classify_labels`] are pure and covered by unit tests, but
//! "the restricted Role is sufficient" and "kube-rs reads the label back" need
//! an apiserver. Both were verified against kind (k8s v1.34) by minting a
//! kubeconfig for a ServiceAccount holding ONLY the rule above and calling
//! [`observe_namespace`]:
//!
//! | namespace state              | observed                             |
//! |------------------------------|--------------------------------------|
//! | no label                     | `Unmanaged { observed: None }`       |
//! | `djinn.io/kueue-managed=false` | `Unmanaged { observed: Some("false") }` |
//! | `djinn.io/kueue-managed=true`  | `Managed`                            |
//! | label `=true`, RoleBinding deleted | `Unverifiable { .. 403 Forbidden }` |
//!
//! The last row is the one that matters most: losing the read permission must
//! never be mistaken for a confirmed label. It disarms.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::Namespace;
use kube::api::Api;

/// Namespace label Kueue requires in order to capture Jobs in that namespace.
///
/// Applied by `deploy/helm/djinn/templates/namespace.yaml` when `kueue.armed` is
/// true, or by the operator's own tooling when `namespace.create` is false.
pub const LABEL_KUEUE_MANAGED: &str = "djinn.io/kueue-managed";

/// What the live Namespace object says about Kueue management.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceKueueStatus {
    /// `djinn.io/kueue-managed=true` is present on the live object.
    Managed,
    /// The label is absent, or present with a value other than `true`. Arming
    /// into this namespace hangs every build Job.
    Unmanaged {
        /// The observed label value, if the key was present at all.
        observed: Option<String>,
    },
    /// The namespace could not be read, so nothing is known either way.
    Unverifiable {
        /// Human-readable cause, surfaced in the disarm log line.
        reason: String,
    },
}

/// What the server should do with the requested arming state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KueuePreflightOutcome {
    /// Arming was never requested; no namespace read was performed.
    NotRequested,
    /// Arming was requested and the namespace label was confirmed. Stay armed.
    Armed,
    /// Arming was requested but not proven. Disarm and log `reason`.
    Disarmed {
        /// Operator-facing explanation of why arming was refused.
        reason: String,
    },
}

impl KueuePreflightOutcome {
    /// Whether the renderers may keep stamping Kueue admission onto build Jobs.
    #[must_use]
    pub fn armed(&self) -> bool {
        matches!(self, Self::Armed)
    }
}

/// Classify a Namespace's labels.
///
/// Split out from the API call so the decision table is testable without a
/// cluster.
#[must_use]
pub fn classify_labels(labels: Option<&BTreeMap<String, String>>) -> NamespaceKueueStatus {
    let observed = labels
        .and_then(|labels| labels.get(LABEL_KUEUE_MANAGED))
        .cloned();
    match observed.as_deref() {
        Some("true") => NamespaceKueueStatus::Managed,
        _ => NamespaceKueueStatus::Unmanaged { observed },
    }
}

/// The whole decision table: requested arming plus observed namespace state in,
/// final arming state out.
///
/// Pure, so the "arm only on positive proof" rule can be asserted directly
/// rather than inferred from log output.
#[must_use]
pub fn decide(requested_armed: bool, status: &NamespaceKueueStatus) -> KueuePreflightOutcome {
    if !requested_armed {
        return KueuePreflightOutcome::NotRequested;
    }
    match status {
        NamespaceKueueStatus::Managed => KueuePreflightOutcome::Armed,
        NamespaceKueueStatus::Unmanaged { observed } => KueuePreflightOutcome::Disarmed {
            reason: format!(
                "namespace is not labelled {LABEL_KUEUE_MANAGED}=true (observed: {observed:?}), \
                 so Kueue would never capture the suspended build Jobs arming creates"
            ),
        },
        NamespaceKueueStatus::Unverifiable { reason } => KueuePreflightOutcome::Disarmed {
            reason: format!(
                "could not read the namespace to confirm {LABEL_KUEUE_MANAGED}=true ({reason}); \
                 arming is only safe on positive proof"
            ),
        },
    }
}

/// Number of namespace reads attempted before giving up.
///
/// A single transient apiserver error at boot must not cost the deployment its
/// quota enforcement until the next restart, but the retry budget stays small:
/// a genuinely absent label is not going to appear.
const READ_ATTEMPTS: u32 = 3;
/// Backoff between namespace read attempts.
const READ_BACKOFF: Duration = Duration::from_millis(500);

/// Read `namespace` and classify its Kueue-management label, retrying transient
/// failures a bounded number of times.
pub async fn observe_namespace(client: &kube::Client, namespace: &str) -> NamespaceKueueStatus {
    let api: Api<Namespace> = Api::all(client.clone());
    let mut last_error = String::new();
    for attempt in 1..=READ_ATTEMPTS {
        match api.get(namespace).await {
            Ok(object) => return classify_labels(object.metadata.labels.as_ref()),
            Err(error) => {
                last_error = error.to_string();
                tracing::debug!(
                    namespace,
                    attempt,
                    error = %last_error,
                    "kueue preflight: namespace read failed"
                );
                if attempt < READ_ATTEMPTS {
                    tokio::time::sleep(READ_BACKOFF).await;
                }
            }
        }
    }
    NamespaceKueueStatus::Unverifiable {
        reason: format!("{READ_ATTEMPTS} namespace GETs failed, last error: {last_error}"),
    }
}

/// Run the preflight end to end and log the outcome.
///
/// Returns the outcome so the caller can latch it; see
/// [`crate::config::disarm_kueue_globally`].
pub async fn run(
    client: &kube::Client,
    namespace: &str,
    requested_armed: bool,
) -> KueuePreflightOutcome {
    if !requested_armed {
        return KueuePreflightOutcome::NotRequested;
    }
    let status = observe_namespace(client, namespace).await;
    let outcome = decide(requested_armed, &status);
    match &outcome {
        KueuePreflightOutcome::Armed => tracing::info!(
            namespace,
            label = LABEL_KUEUE_MANAGED,
            "kueue preflight: namespace is Kueue-managed, arming confirmed"
        ),
        KueuePreflightOutcome::Disarmed { reason } => tracing::error!(
            namespace,
            reason = %reason,
            "kueue preflight: REFUSING to arm Kueue — build Jobs will render unsuspended \
             (no Kueue quota) instead of hanging forever. Label the namespace with \
             `kubectl label namespace <ns> djinn.io/kueue-managed=true` and restart, or set \
             kueue.armed=false."
        ),
        // Unreachable: `requested_armed` was checked above.
        KueuePreflightOutcome::NotRequested => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn managed_only_when_the_label_reads_true() {
        assert_eq!(
            classify_labels(Some(&labels(&[(LABEL_KUEUE_MANAGED, "true")]))),
            NamespaceKueueStatus::Managed
        );
        // A namespace that merely has SOME labels is not managed. Guards against
        // a classifier that keys off label presence rather than this key.
        assert_eq!(
            classify_labels(Some(&labels(&[("app.kubernetes.io/name", "djinn")]))),
            NamespaceKueueStatus::Unmanaged { observed: None }
        );
        assert_eq!(
            classify_labels(None),
            NamespaceKueueStatus::Unmanaged { observed: None }
        );
    }

    #[test]
    fn a_non_true_label_value_is_not_management() {
        // `kubectl label ns x djinn.io/kueue-managed=false` is not "close
        // enough": Kueue reads the value, and so must this.
        for value in ["false", "True", "1", "yes", ""] {
            assert_eq!(
                classify_labels(Some(&labels(&[(LABEL_KUEUE_MANAGED, value)]))),
                NamespaceKueueStatus::Unmanaged {
                    observed: Some(value.to_string())
                },
                "value {value:?} must not count as Kueue management"
            );
        }
    }

    #[test]
    fn disarmed_state_is_never_touched_by_the_preflight() {
        // The preflight must not be able to ARM something the operator did not
        // ask to arm, whatever the namespace says.
        for status in [
            NamespaceKueueStatus::Managed,
            NamespaceKueueStatus::Unmanaged { observed: None },
            NamespaceKueueStatus::Unverifiable {
                reason: "boom".into(),
            },
        ] {
            assert_eq!(decide(false, &status), KueuePreflightOutcome::NotRequested);
            assert!(!decide(false, &status).armed());
        }
    }

    #[test]
    fn arming_survives_only_a_confirmed_label() {
        assert_eq!(
            decide(true, &NamespaceKueueStatus::Managed),
            KueuePreflightOutcome::Armed
        );
        assert!(decide(true, &NamespaceKueueStatus::Managed).armed());
    }

    #[test]
    fn an_unlabelled_namespace_disarms_and_says_why() {
        // This is the exact configuration the task exists to close: an operator
        // claimed `kueue.namespaceLabelledExternally=true` and was wrong.
        let outcome = decide(true, &NamespaceKueueStatus::Unmanaged { observed: None });
        assert!(
            !outcome.armed(),
            "an unlabelled namespace must not stay armed"
        );
        let KueuePreflightOutcome::Disarmed { reason } = outcome else {
            panic!("expected a disarm");
        };
        assert!(
            reason.contains(LABEL_KUEUE_MANAGED),
            "the disarm reason must name the missing label: {reason}"
        );
    }

    #[test]
    fn an_unreadable_namespace_disarms_rather_than_assuming() {
        // RBAC stripped, apiserver down, whatever: no proof is not proof.
        let outcome = decide(
            true,
            &NamespaceKueueStatus::Unverifiable {
                reason: "forbidden".into(),
            },
        );
        assert!(
            !outcome.armed(),
            "an unverifiable namespace must not stay armed"
        );
        let KueuePreflightOutcome::Disarmed { reason } = outcome else {
            panic!("expected a disarm");
        };
        assert!(
            reason.contains("forbidden"),
            "the disarm reason must carry the underlying cause: {reason}"
        );
    }
}
