use super::actor::CoordinatorActor;
use djinn_db::TypedEvidenceRepository;

impl CoordinatorActor {
    /// Replay the same durable raw V1 deliveries consumed by the live event
    /// path. Inventory is rooted in typed attempts, so it remains available
    /// after `submit_return_v1` has atomically cleared a compatibility link.
    pub(super) async fn recover_terminal_linked_spike_evidence(
        &mut self,
    ) -> Vec<djinn_db::TribunalEvidenceReturnResultV1> {
        let repo = TypedEvidenceRepository::new(self.db.clone());
        let deliveries = match repo.terminal_return_v1_deliveries().await {
            Ok(deliveries) => deliveries,
            Err(error) => {
                tracing::warn!(%error, "CoordinatorActor: typed evidence delivery recovery inventory failed");
                return Vec::new();
            }
        };
        self.ingest_terminal_return_v1_deliveries(deliveries).await
    }

    /// Re-drive raw activities observed before their producer task settled.
    pub(super) async fn recover_terminal_linked_spike_evidence_for_task(
        &mut self,
        spike_task_id: &str,
    ) -> Vec<djinn_db::TribunalEvidenceReturnResultV1> {
        let repo = TypedEvidenceRepository::new(self.db.clone());
        let deliveries = match repo
            .terminal_return_v1_deliveries_for_task(spike_task_id)
            .await
        {
            Ok(deliveries) => deliveries,
            Err(error) => {
                tracing::warn!(%error, task_id=%spike_task_id, "CoordinatorActor: terminal typed evidence delivery lookup failed");
                return Vec::new();
            }
        };
        self.ingest_terminal_return_v1_deliveries(deliveries).await
    }

    async fn ingest_terminal_return_v1_deliveries(
        &mut self,
        deliveries: Vec<djinn_db::TerminalTypedEvidenceReturnDelivery>,
    ) -> Vec<djinn_db::TribunalEvidenceReturnResultV1> {
        let mut results = Vec::new();
        for delivery in deliveries {
            if let Some(result) = self
                .ingest_raw_tribunal_evidence_return_v1(&delivery.spike_task_id, &delivery.payload)
                .await
            {
                results.push(result);
            }
        }
        results
    }
}
