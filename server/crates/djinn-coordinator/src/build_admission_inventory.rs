//! Conservative Kubernetes inventory classification and reconciliation.
use crate::build_admission::{BuildAdmissionController, BuildAdmissionMode};
use djinn_db::{
    AdmissionDomain, AdmissionJournalKey, AdmissionRecoveryResult, AdmissionState,
    AdmissionWorkloadKind, AdoptLiveAdmissionInput, TerminalAdmissionInput,
};
use djinn_k8s::{
    LABEL_ADMISSION_DOMAIN, LABEL_ADMISSION_GENERATION, LABEL_ADMISSION_WORK_ID, UidGetResult,
    WorkloadInventory, WorkloadRecord, has_canonical_warm_signature,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::Mutex;
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
    pub blockers: Vec<String>,
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
    serial: Mutex<()>,
}
impl BuildAdmissionReconciler {
    pub fn new(
        controller: Arc<BuildAdmissionController>,
        inventory: Arc<dyn WorkloadInventory>,
    ) -> Self {
        Self {
            controller,
            inventory,
            serial: Mutex::new(()),
        }
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
                return InventoryReport {
                    blockers: vec![e],
                    ..Default::default()
                };
            }
        };
        let mut out = InventoryReport::default();
        let mut cs = Vec::new();
        let mut ids = HashSet::new();
        for r in records {
            match classify(&r) {
                Ok(Some(c)) => {
                    let id = format!("{:?}:{}:{}", c.key.domain, c.key.work_id, c.key.generation);
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
                Err(e) => out.blockers.push(e.to_string()),
            }
        }
        let active = self
            .controller
            .journal()
            .list_active_rows()
            .await
            .unwrap_or_default();
        let by: HashMap<_, _> = cs
            .iter()
            .map(|c| {
                (
                    format!("{:?}:{}:{}", c.key.domain, c.key.work_id, c.key.generation),
                    c,
                )
            })
            .collect();
        for row in &active {
            if row.state != AdmissionState::Live {
                continue;
            }
            let id = format!(
                "{:?}:{}:{}",
                row.key.domain, row.key.work_id, row.key.generation
            );
            let proof = if let Some(c) = by.get(&id) {
                c.object.terminal && c.object.uid.as_deref() == row.object_uid.as_deref()
            } else if let Some(uid) = row.object_uid.as_deref() {
                self.inventory
                    .get_uid(djinn_k8s::WorkloadObjectKind::Job, &row.object_name, uid)
                    .await
                    == UidGetResult::NotFound
            } else {
                false
            };
            if proof
                && self
                    .controller
                    .journal()
                    .mark_terminal(&TerminalAdmissionInput {
                        key: row.key.clone(),
                        object_uid: row.object_uid.clone(),
                    })
                    .await
                    .is_ok()
            {
                out.released += 1;
                self.controller.release_notifier().notify_one()
            }
        }
        if out.blockers.is_empty() {
            let rows = self
                .controller
                .journal()
                .list_active_rows()
                .await
                .unwrap_or_default();
            let recovery = AdmissionRecoveryResult {
                retired_reserved: 0,
                marked_create_unknown: 0,
                active_rows: rows,
            };
            let _ = self
                .controller
                .seed_from_recovery(&recovery, &mut |_| true)
                .await;
            self.controller.mark_inventory_ready()
        } else {
            self.controller.mark_inventory_pending()
        }
        out
    }
}
