//! C4: the coordinator-leader half of the `direct_delivery_v1` activation fence.
//!
//! Two things happen here, both from the leader tick:
//!
//! 1. **Capability advertisement.** This process writes one
//!    `direct_delivery_process_capabilities` row per capability it can actually
//!    provide, at the generation activation would move the epoch to. That write
//!    is unconditional — a process must be countable by the census whether or
//!    not anyone has asked for activation — and it is the only production
//!    writer of that relation.
//!
//! 2. **Activation.** Only when an operator has explicitly requested it does
//!    the leader run [`DirectDeliveryActivationRepository::activate`]. That
//!    request is the reason the shipped default stays disabled at rest: the
//!    census alone would otherwise activate direct delivery on the first
//!    deployment that satisfied it, which is precisely the "mixed behaviour
//!    enabled by accident" the proposal forbids.
//!
//! # What an advertised capability actually asserts
//!
//! `schema` and `repository` are **live probes**: they read the persisted C0
//! relations and the epoch row through
//! [`DirectDeliveryCapabilityRepository`], so a binary talking to an old
//! database cannot advertise them.
//!
//! `provider`, `orchestrator` and `consumer_cutover` are **compiled-contract
//! identities** owned by the crate that implements each contract
//! ([`djinn_provider::github_api::DIRECT_DELIVERY_REF_CONTRACT`],
//! [`crate::direct_delivery::DIRECT_DELIVERY_ORCHESTRATOR_CONTRACT`],
//! [`crate::direct_delivery::DIRECT_DELIVERY_CONSUMER_CUTOVER_CONTRACT`]). A
//! binary built before those contracts existed does not define the constant at
//! all, so this module would not compile against it.
//! `direct_delivery_activation_matrix` additionally enumerates the production
//! sources behind each of them, so the declarations cannot outlive the code
//! they name.
//!
//! # Scope of the census population
//!
//! The census is taken over `coordinator_incarnations`: the leader-elected
//! server processes, which are the only processes that reserve, activate,
//! append to, or integrate a direct delivery. Taskrun worker pods are *not* in
//! the population. They are consumers — `djinn-agent`'s task-PR-open body is
//! gated by the same `task_pr_eligibility` call — but they neither register an
//! incarnation nor advertise, so a worker pod running an image that predates
//! that gate is not something this fence can observe. Activation therefore also
//! requires an explicit operator request, which is where that judgement is
//! made; the fence proves the coordinator fleet is ready, not the pod fleet.

use djinn_core::models::{DirectDeliveryCapability, DirectDeliveryEpoch};
use djinn_db::{
    ActivateDirectDeliveryEpochInput, ActivateDirectDeliveryEpochResult, Database,
    DirectDeliveryActivationRefusal, DirectDeliveryActivationRepository,
    DirectDeliveryCapabilityRepository, DirectDeliverySchemaCapability, SettingsRepository,
};

/// Explicit operator request for activation. Absent or anything other than
/// `true` means the leader advertises and stops.
pub const ACTIVATION_REQUEST_SETTING_KEY: &str = "direct_delivery_v1.activation_requested";

/// The value the request setting must carry, exactly.
pub const ACTIVATION_REQUEST_SETTING_VALUE: &str = "true";

/// What one leader activation pass did. Every variant is observable by the
/// matrix; none of them is inferred from a log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationPassOutcome {
    /// The epoch could not even be probed (old schema, missing epoch row,
    /// unreadable state). Nothing was advertised and nothing was activated.
    ContractUnavailable {
        detail: String,
    },
    /// Capabilities were advertised; no operator request exists.
    NotRequested {
        advertised: Vec<DirectDeliveryCapability>,
    },
    /// Already active. Advertisement still refreshes so a later generation can
    /// be censused.
    AlreadyActive {
        generation: i64,
    },
    /// Requested, but the activation transaction refused.
    Refused {
        advertised: Vec<DirectDeliveryCapability>,
        refusal: DirectDeliveryActivationRefusal,
    },
    Activated {
        generation: i64,
    },
    /// A crash-retry of the same activation observed it already applied.
    Replayed {
        generation: i64,
    },
}

/// Capabilities this exact binary, against this exact database, can provide.
///
/// A capability absent from this list is never advertised, so the census gap it
/// leaves is what refuses activation.
pub async fn observed_capabilities(db: &Database) -> Vec<DirectDeliveryCapability> {
    let probe = DirectDeliveryCapabilityRepository::new(db.clone())
        .probe()
        .await;
    let mut capabilities = Vec::with_capacity(DirectDeliveryCapability::ALL.len());
    match probe {
        // The C0 relations are all present: this binary can read the schema.
        Ok(
            DirectDeliverySchemaCapability::SupportedDisabled { .. }
            | DirectDeliverySchemaCapability::SupportedActive { .. },
        ) => {
            capabilities.push(DirectDeliveryCapability::Schema);
            // The epoch row itself parsed into the closed typed contract, so
            // the repository layer can transact against it.
            capabilities.push(DirectDeliveryCapability::Repository);
        }
        // `UnknownEpochState` still proves the relations exist, but not that
        // this binary understands the persisted state, so `repository` is
        // withheld.
        Ok(DirectDeliverySchemaCapability::UnknownEpochState { .. }) => {
            capabilities.push(DirectDeliveryCapability::Schema);
        }
        Ok(DirectDeliverySchemaCapability::MissingSchema { .. })
        | Ok(DirectDeliverySchemaCapability::MissingEpoch)
        | Err(_) => {}
    }
    if djinn_provider::github_api::DIRECT_DELIVERY_REF_CONTRACT == DirectDeliveryEpoch::NAME {
        capabilities.push(DirectDeliveryCapability::Provider);
    }
    if crate::direct_delivery::DIRECT_DELIVERY_ORCHESTRATOR_CONTRACT == DirectDeliveryEpoch::NAME {
        capabilities.push(DirectDeliveryCapability::Orchestrator);
    }
    if crate::direct_delivery::DIRECT_DELIVERY_CONSUMER_CUTOVER_CONTRACT
        == DirectDeliveryEpoch::NAME
    {
        capabilities.push(DirectDeliveryCapability::ConsumerCutover);
    }
    capabilities
}

/// The census liveness threshold, expressed as the ISO instant a
/// `coordinator_incarnations` row must have renewed at or after.
///
/// It is deliberately the same window the orphaned-attempt reaper uses, so
/// "live" cannot mean one thing to the activation fence and another to
/// recovery: any process the reaper still treats as a live dispatch owner is a
/// process this census must count.
pub fn live_since_iso() -> Option<String> {
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    (time::OffsetDateTime::now_utc()
        - time::Duration::seconds(crate::health::COORDINATOR_LIVENESS_THRESHOLD_SECS))
    .format(&format)
    .ok()
}

/// One leader pass: advertise, then activate if and only if an operator asked.
///
/// `process_incarnation_id` must be the same identity this process registered
/// in `coordinator_incarnations`, because that table is the census population.
pub async fn run_direct_delivery_activation_pass(
    db: &Database,
    events: djinn_core::events::EventBus,
    process_incarnation_id: &str,
) -> ActivationPassOutcome {
    let epoch = match DirectDeliveryCapabilityRepository::new(db.clone())
        .probe()
        .await
    {
        Ok(DirectDeliverySchemaCapability::SupportedDisabled { epoch }) => epoch,
        Ok(DirectDeliverySchemaCapability::SupportedActive { epoch }) => {
            // Keep advertising at the *next* generation so a future activation
            // still has a complete census to read.
            advertise(db, process_incarnation_id, epoch.generation + 1).await;
            return ActivationPassOutcome::AlreadyActive {
                generation: epoch.generation,
            };
        }
        Ok(other) => {
            return ActivationPassOutcome::ContractUnavailable {
                detail: format!("{other:?}"),
            };
        }
        Err(error) => {
            return ActivationPassOutcome::ContractUnavailable {
                detail: error.to_string(),
            };
        }
    };

    let target_generation = epoch.generation + 1;
    let advertised = advertise(db, process_incarnation_id, target_generation).await;

    let requested = SettingsRepository::new(db.clone(), events)
        .get(ACTIVATION_REQUEST_SETTING_KEY)
        .await
        .ok()
        .flatten()
        .is_some_and(|setting| setting.value.trim() == ACTIVATION_REQUEST_SETTING_VALUE);
    if !requested {
        return ActivationPassOutcome::NotRequested { advertised };
    }

    let Some(live_since) = live_since_iso() else {
        return ActivationPassOutcome::ContractUnavailable {
            detail: "failed to format the census liveness threshold".into(),
        };
    };
    match DirectDeliveryActivationRepository::new(db.clone())
        .activate(&ActivateDirectDeliveryEpochInput {
            expected_generation: epoch.generation,
            live_since,
        })
        .await
    {
        Ok(ActivateDirectDeliveryEpochResult::Activated(activated)) => {
            tracing::warn!(
                generation = activated.generation,
                "direct_delivery_v1 activated: task PRs are no longer opened for \
                 proposal-owned tasks"
            );
            ActivationPassOutcome::Activated {
                generation: activated.generation,
            }
        }
        Ok(ActivateDirectDeliveryEpochResult::Replayed(replayed)) => {
            ActivationPassOutcome::Replayed {
                generation: replayed.generation,
            }
        }
        Ok(ActivateDirectDeliveryEpochResult::Refused(refusal)) => {
            tracing::info!(
                refusal = refusal.as_str(),
                detail = %refusal,
                "direct_delivery_v1 activation refused; epoch remains disabled"
            );
            ActivationPassOutcome::Refused {
                advertised,
                refusal,
            }
        }
        Err(error) => ActivationPassOutcome::ContractUnavailable {
            detail: error.to_string(),
        },
    }
}

/// The sole production writer of `direct_delivery_process_capabilities`.
async fn advertise(
    db: &Database,
    process_incarnation_id: &str,
    target_generation: i64,
) -> Vec<DirectDeliveryCapability> {
    let capabilities = observed_capabilities(db).await;
    if let Err(error) = DirectDeliveryActivationRepository::new(db.clone())
        .advertise_capabilities(process_incarnation_id, target_generation, &capabilities)
        .await
    {
        tracing::warn!(
            %error,
            process_incarnation_id,
            target_generation,
            "failed to advertise direct-delivery capabilities; this process will \
             leave a census gap and activation will refuse"
        );
    }
    capabilities
}
