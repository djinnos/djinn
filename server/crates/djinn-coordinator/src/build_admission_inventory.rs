//! Conservative Kubernetes inventory classification and reconciliation.
use crate::build_admission::{BuildAdmissionController, BuildAdmissionMode};
use djinn_db::{
    AdmissionDomain, AdmissionJournalKey, AdmissionJournalRow, AdmissionRecoveryResult,
    AdmissionState, AdmissionWorkloadKind, AdoptLiveAdmissionInput, ReclaimAbsentInput,
    ReclaimAbsentOutcome, TerminalAdmissionInput,
};
use djinn_k8s::{
    LABEL_ADMISSION_DOMAIN, LABEL_ADMISSION_GENERATION, LABEL_ADMISSION_WORK_ID, ObjectPresence,
    UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
    has_canonical_warm_signature,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

/// How long an occupying row must sit untouched before its object's absence is
/// allowed to retire it.
///
/// This is a guard, never the reason. A Kubernetes create that a since-dead
/// process POSTed can still be admitted by the API server shortly after that
/// process is gone, so a row that was written moments ago is not yet safe to
/// judge by a LIST/GET. Five minutes is far beyond any create-admission window
/// and far below the lifetime of the stale populations this reclaims.
pub const DEFAULT_RECLAIM_SETTLE_WINDOW: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedWorkload {
    pub key: AdmissionJournalKey,
    pub kind: AdmissionWorkloadKind,
    pub object: WorkloadRecord,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InventoryReport {
    pub adopted: usize,
    pub released: usize,
    /// Occupying rows in a pre-Live state that were retired because their
    /// Kubernetes object was proven absent.
    pub reclaimed: usize,
    /// Rows whose object was proven absent, counted before any reclamation.
    /// This is the size of the stale population, not the size of the fix.
    pub stale: usize,
    /// Reclamations refused because the row changed after its absence proof.
    pub fenced: usize,
    pub blockers: Vec<String>,
}
fn identity(key: &AdmissionJournalKey) -> String {
    format!("{:?}:{}:{}", key.domain, key.work_id, key.generation)
}
fn domain(v: &str) -> Option<AdmissionDomain> {
    match v {
        "task_observation" => Some(AdmissionDomain::TaskObservation),
        "warm_build" => Some(AdmissionDomain::WarmBuild),
        "invocation_build" => Some(AdmissionDomain::InvocationBuild),
        _ => None,
    }
}
fn classify(r: &WorkloadRecord) -> Result<Option<ClassifiedWorkload>, String> {
    let l = &r.labels;
    if l.contains_key(LABEL_ADMISSION_DOMAIN)
        || l.contains_key(LABEL_ADMISSION_WORK_ID)
        || l.contains_key(LABEL_ADMISSION_GENERATION)
    {
        let d = l
            .get(LABEL_ADMISSION_DOMAIN)
            .and_then(|v| domain(v))
            .ok_or_else(|| format!("{}: invalid domain", r.name))?;
        let w = l
            .get(LABEL_ADMISSION_WORK_ID)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{}: missing identity", r.name))?
            .clone();
        let g = l
            .get(LABEL_ADMISSION_GENERATION)
            .and_then(|v| v.parse().ok())
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("{}: invalid generation", r.name))?;
        let k = if d == AdmissionDomain::WarmBuild {
            AdmissionWorkloadKind::Warm
        } else {
            AdmissionWorkloadKind::Task
        };
        return Ok(Some(ClassifiedWorkload {
            key: AdmissionJournalKey {
                domain: d,
                work_id: w,
                generation: g,
            },
            kind: k,
            object: r.clone(),
        }));
    }
    let task_candidate = l.contains_key("djinn.app/task-run-id")
        || r.name.starts_with("djinn-taskrun-")
        || l.get("djinn.app/component")
            .is_some_and(|value| value == "task-run-worker");
    if task_candidate {
        let work_id = l
            .get("djinn.app/task-run-id")
            .filter(|value| !value.is_empty())
            .cloned()
            .or_else(|| r.uid.as_ref().map(|uid| format!("legacy-task:{uid}")))
            .ok_or_else(|| format!("{}: unstable UID", r.name))?;
        return Ok(Some(ClassifiedWorkload {
            key: AdmissionJournalKey {
                domain: AdmissionDomain::TaskObservation,
                work_id,
                generation: 0,
            },
            kind: AdmissionWorkloadKind::Task,
            object: r.clone(),
        }));
    }
    if l.get("djinn.app/warm").is_some_and(|v| v == "true") || has_canonical_warm_signature(r) {
        let u = r
            .uid
            .as_deref()
            .ok_or_else(|| format!("{}: unstable UID", r.name))?;
        return Ok(Some(ClassifiedWorkload {
            key: AdmissionJournalKey {
                domain: AdmissionDomain::WarmBuild,
                work_id: format!("legacy-warm:{u}"),
                generation: 0,
            },
            kind: AdmissionWorkloadKind::Warm,
            object: r.clone(),
        }));
    }
    if (r.name.starts_with("djinn-") || l.keys().any(|k| k.starts_with("djinn.app/")))
        && !r.terminal
        && !r.images.is_empty()
    {
        return Err(format!("{}: unclassifiable build workload", r.name));
    }
    Ok(None)
}
pub struct BuildAdmissionReconciler {
    controller: Arc<BuildAdmissionController>,
    inventory: Arc<dyn WorkloadInventory>,
    settle_window: Duration,
    serial: Mutex<()>,
}
impl BuildAdmissionReconciler {
    pub fn new(
        controller: Arc<BuildAdmissionController>,
        inventory: Arc<dyn WorkloadInventory>,
    ) -> Self {
        Self::with_settle_window(controller, inventory, DEFAULT_RECLAIM_SETTLE_WINDOW)
    }

    /// Reconcile with an explicit settle window. Tests use a zero window to
    /// exercise reclamation without sleeping; production uses the default.
    pub fn with_settle_window(
        controller: Arc<BuildAdmissionController>,
        inventory: Arc<dyn WorkloadInventory>,
        settle_window: Duration,
    ) -> Self {
        Self {
            controller,
            inventory,
            settle_window,
            serial: Mutex::new(()),
        }
    }
    /// Whether a pre-Live occupying row's Kubernetes object is provably gone.
    ///
    /// Every clause is a fence that must open; none of them is a heuristic
    /// about how old the row looks:
    ///
    /// * the row's creator epoch is not this process, so no in-process dispatch
    ///   can still be mid-create for it — the only process that could have
    ///   finished this create is gone;
    /// * the row has settled, so the API server can no longer be admitting a
    ///   create the dead process POSTed;
    /// * the authoritative LIST that just succeeded contains no object under
    ///   this row's name, and it did not classify to a live workload;
    /// * a direct GET, taken now and independently of the LIST snapshot,
    ///   answers that no object with that name exists. `Uncertain` — a
    ///   transport or permission failure — is never proof, so a degraded API
    ///   server leaves every row occupying.
    async fn is_reclaimable(
        &self,
        row: &AdmissionJournalRow,
        classified: &Option<&ClassifiedWorkload>,
        listed_names: &HashSet<String>,
        settled: &HashMap<String, bool>,
    ) -> bool {
        if row.creator_server_epoch == self.controller.server_epoch() {
            return false;
        }
        if !settled.get(&identity(&row.key)).copied().unwrap_or(false) {
            return false;
        }
        if classified.is_some() || listed_names.contains(&row.object_name) {
            return false;
        }
        self.inventory
            .presence(WorkloadObjectKind::Job, &row.object_name)
            .await
            == ObjectPresence::Absent
    }

    pub async fn reconcile(&self) -> InventoryReport {
        let _g = self.serial.lock().await;
        if self.controller.mode() == BuildAdmissionMode::Off {
            return InventoryReport::default();
        }
        let records = match self.inventory.list().await {
            Ok(v) => v,
            Err(e) => {
                self.controller.mark_inventory_pending();
                self.controller.publish_metrics().await;
                return InventoryReport {
                    blockers: vec![e],
                    ..Default::default()
                };
            }
        };
        let mut out = InventoryReport::default();
        let mut cs = Vec::new();
        let mut ids = HashSet::new();
        // Every Job name the authoritative LIST returned, including records
        // classification skipped or rejected. A row whose object name appears
        // here is never a reclamation candidate, whatever we could or could not
        // make of that object's labels.
        let listed_names: HashSet<String> = records.iter().map(|r| r.name.clone()).collect();
        for r in records {
            match classify(&r) {
                Ok(Some(c)) => {
                    let id = identity(&c.key);
                    if ids.insert(id) {
                        cs.push(c)
                    } else {
                        out.blockers
                            .push(format!("{}: duplicate identity", c.object.name))
                    }
                }
                Ok(None) => {}
                Err(e) => out.blockers.push(e),
            }
        }
        for c in &cs {
            if c.object.terminal {
                continue;
            }
            let Some(uid) = c.object.uid.as_ref() else {
                out.blockers
                    .push(format!("{}: unstable UID", c.object.name));
                continue;
            };
            let x = AdoptLiveAdmissionInput {
                key: c.key.clone(),
                workload_kind: c.kind,
                creator_server_epoch: self.controller.server_epoch().into(),
                object_name: c.object.name.clone(),
                object_uid: uid.clone(),
            };
            match self.controller.journal().adopt_live(&x).await {
                Ok(_) => out.adopted += 1,
                Err(e) => {
                    self.controller.mark_journal_unhealthy();
                    out.blockers.push(e.to_string());
                }
            }
        }
        let active = match self
            .controller
            .journal()
            .list_active_rows_with_settlement(self.settle_window.as_secs() as i64)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                self.controller.mark_journal_unhealthy();
                self.controller.mark_inventory_pending();
                out.blockers.push(error.to_string());
                self.controller.publish_metrics().await;
                return out;
            }
        };
        let settled: HashMap<String, bool> = active
            .iter()
            .map(|(row, settled)| (identity(&row.key), *settled))
            .collect();
        let active: Vec<AdmissionJournalRow> = active.into_iter().map(|(row, _)| row).collect();
        let by: HashMap<_, _> = cs.iter().map(|c| (identity(&c.key), c)).collect();
        for row in &active {
            let id = identity(&row.key);
            if row.state == AdmissionState::Live {
                let proof = if let Some(c) = by.get(&id) {
                    c.object.terminal && c.object.uid.as_deref() == row.object_uid.as_deref()
                } else if let Some(uid) = row.object_uid.as_deref() {
                    self.inventory
                        .get_uid(WorkloadObjectKind::Job, &row.object_name, uid)
                        .await
                        == UidGetResult::NotFound
                } else {
                    false
                };
                if proof {
                    out.stale += 1;
                    match self
                        .controller
                        .journal()
                        .mark_terminal(&TerminalAdmissionInput {
                            key: row.key.clone(),
                            object_uid: row.object_uid.clone(),
                        })
                        .await
                    {
                        Ok(_) => {
                            out.released += 1;
                            self.controller.release_notifier().notify_one();
                        }
                        Err(error) => {
                            self.controller.mark_journal_unhealthy();
                            out.blockers.push(error.to_string());
                        }
                    }
                }
                continue;
            }
            // Reserved / CreateInFlight / CreateUnknown. Recovery retires
            // Reserved rows and converts CreateInFlight into occupying
            // CreateUnknown, and adoption rescues a CreateUnknown row whose
            // object still exists — but nothing at all terminalizes one whose
            // object is gone. Those rows occupy the shared cap forever, which
            // is how a fleet accumulates a stale population large enough to
            // deny every admission the moment the cap is armed.
            if !self
                .is_reclaimable(row, &by.get(&id).copied(), &listed_names, &settled)
                .await
            {
                continue;
            }
            out.stale += 1;
            let input = ReclaimAbsentInput {
                key: row.key.clone(),
                observed_state: row.state,
                observed_creator_server_epoch: row.creator_server_epoch.clone(),
                observed_object_name: row.object_name.clone(),
                observed_object_uid: row.object_uid.clone(),
            };
            match self
                .controller
                .journal()
                .reclaim_absent_object(&input)
                .await
            {
                Ok(ReclaimAbsentOutcome::Reclaimed(_)) => {
                    out.reclaimed += 1;
                    self.controller.release_notifier().notify_one();
                }
                Ok(ReclaimAbsentOutcome::AlreadyTerminal(_)) => {}
                Ok(ReclaimAbsentOutcome::Fenced { reason }) => {
                    out.fenced += 1;
                    tracing::warn!(
                        work_id = %row.key.work_id,
                        generation = row.key.generation,
                        object = %row.object_name,
                        %reason,
                        "build_admission: refused to reclaim an admission row that changed \
                         after its absence proof"
                    );
                }
                Err(error) => {
                    self.controller.mark_journal_unhealthy();
                    out.blockers.push(error.to_string());
                }
            }
        }
        if out.blockers.is_empty() {
            let rows = match self.controller.journal().list_active_rows().await {
                Ok(rows) => rows,
                Err(error) => {
                    self.controller.mark_journal_unhealthy();
                    self.controller.mark_inventory_pending();
                    out.blockers.push(error.to_string());
                    self.controller.publish_metrics().await;
                    return out;
                }
            };
            let recovery = AdmissionRecoveryResult {
                retired_reserved: 0,
                marked_create_unknown: 0,
                active_rows: rows,
            };
            if let Err(error) = self
                .controller
                .seed_from_recovery(&recovery, &mut |_| true)
                .await
            {
                self.controller.mark_journal_unhealthy();
                self.controller.mark_inventory_pending();
                out.blockers.push(error.to_string());
            } else {
                self.controller.mark_inventory_ready();
            }
        } else {
            self.controller.mark_inventory_pending();
        }
        // Adoption may have changed durable occupancy (valid live workloads
        // were adopted into the journal before blockers were decided); refresh
        // the occupied gauge and health signals regardless of whether blockers
        // remain. `seed_from_recovery` publishes internally when there are no
        // blockers, but the blockers branch and any adoption under partial
        // inventory still need this explicit publication.
        self.controller.publish_metrics().await;
        // Publish the size of the stale population itself, loudly when it is
        // large enough to have wedged the cap. Discovering this by reading
        // thousands of per-transition warn lines is not an operating model.
        self.controller
            .publish_reconciliation(out.stale, out.released + out.reclaimed, out.fenced);
        out
    }
}
