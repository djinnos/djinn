//! Read-only schema/epoch probe used before any direct-delivery mutation.

use djinn_core::models::{DirectDeliveryEpoch, DirectDeliveryEpochState};
use serde::{Deserialize, Serialize};

use crate::database::Database;
use crate::error::DbResult;

const REQUIRED_RELATIONS: [&str; 5] = [
    "proposal_build_attempts",
    "direct_delivery_epochs",
    "direct_delivery_process_capabilities",
    "task_deliveries",
    "direct_delivery_leases",
];

/// Every result is non-authorizing except `SupportedActive`, which a later
/// activation fence may use only after it has validated all other capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectDeliverySchemaCapability {
    MissingSchema { missing_relations: Vec<String> },
    MissingEpoch,
    UnknownEpochState { state: String, generation: i64 },
    SupportedDisabled { epoch: DirectDeliveryEpoch },
    SupportedActive { epoch: DirectDeliveryEpoch },
}

impl DirectDeliverySchemaCapability {
    #[must_use]
    pub const fn permits_direct_delivery(&self) -> bool {
        matches!(self, Self::SupportedActive { .. })
    }
}

/// Does not call `ensure_initialized`: missing schema is a diagnostic, never a
/// reason to run migrations or to fall back to an implicit delivery mode.
pub struct DirectDeliveryCapabilityRepository {
    db: Database,
}

impl DirectDeliveryCapabilityRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn probe(&self) -> DbResult<DirectDeliverySchemaCapability> {
        let relations: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT to_regclass('public.' || relation_name)::text \
             FROM unnest($1::text[]) AS relation_name",
        )
        .bind(REQUIRED_RELATIONS.as_slice())
        .fetch_all(self.db.pool())
        .await?;
        let missing_relations: Vec<String> = REQUIRED_RELATIONS
            .iter()
            .zip(relations)
            .filter_map(|(name, exists)| exists.is_none().then(|| (*name).to_owned()))
            .collect();
        if !missing_relations.is_empty() {
            return Ok(DirectDeliverySchemaCapability::MissingSchema { missing_relations });
        }

        let row: Option<(String, i64)> =
            sqlx::query_as("SELECT state, generation FROM direct_delivery_epochs WHERE name = $1")
                .bind(DirectDeliveryEpoch::NAME)
                .fetch_optional(self.db.pool())
                .await?;
        let Some((state, generation)) = row else {
            return Ok(DirectDeliverySchemaCapability::MissingEpoch);
        };
        let Ok(state) = state.parse::<DirectDeliveryEpochState>() else {
            return Ok(DirectDeliverySchemaCapability::UnknownEpochState { state, generation });
        };
        let epoch = DirectDeliveryEpoch::new(state, generation)?;
        Ok(match state {
            DirectDeliveryEpochState::Disabled => {
                DirectDeliverySchemaCapability::SupportedDisabled { epoch }
            }
            DirectDeliveryEpochState::Active => {
                DirectDeliverySchemaCapability::SupportedActive { epoch }
            }
        })
    }
}
