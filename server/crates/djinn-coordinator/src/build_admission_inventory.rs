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
    has_canonical_warm_signature, job::LABEL_TASK_RUN_ID,
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
    /// Kubernetes object was proven absent, or proven already finished.
    pub reclaimed: usize,
    /// Rows whose object was proven absent — or proven finished — counted
    /// before any reclamation. This is the size of the stale population, not
    /// the size of the fix.
    pub stale: usize,
    /// Reclamations refused because the row changed after its absence proof.
    pub fenced: usize,
    /// Named per-row journal failures (retirement or adoption), bounded by
    /// [`MAX_NAMED_RECLAIM_FAILURES`]. See `reclaim_failure_count` for the
    /// exact total.
    ///
    /// Deliberately not `blockers`: failing to retire one row leaves that row
    /// occupying, which over-counts capacity. Over-counting is the conservative
    /// direction — it can only deny admissions, never over-admit — so it must
    /// not fail the whole pass and must not gate Enforce on the namespace's
    /// history instead of its current state.
    pub reclaim_failures: Vec<String>,
    /// Exact number of per-row journal failures, named or not.
    pub reclaim_failure_count: usize,
    /// Pass-level failures. A non-empty `blockers` means the pass could not be
    /// trusted at all (unusable listing, unreadable journal, failed seeding)
    /// and Enforce stays fail-closed.
    pub blockers: Vec<String>,
}

/// Bound on how many individual reclaim failures one pass names. The count is
/// always exact; the samples exist so an operator can see *which* row without a
/// log line that grows with the stale population.
pub const MAX_NAMED_RECLAIM_FAILURES: usize = 5;

impl InventoryReport {
    /// Record one per-row journal failure without failing the pass.
    fn row_failure(&mut self, label: &str, error: &str) {
        self.reclaim_failure_count += 1;
        if self.reclaim_failures.len() < MAX_NAMED_RECLAIM_FAILURES {
            self.reclaim_failures.push(format!("{label}: {error}"));
        }
    }

    /// Record one per-row reclamation failure without failing the pass.
    fn reclaim_failure(&mut self, row: &AdmissionJournalRow, error: &str) {
        let label = format!(
            "{}:{}:{}",
            row.key.work_id, row.key.generation, row.object_name
        );
        self.row_failure(&label, error);
    }
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
    // Image builds share the namespace but are not project compiles under the
    // shared task/warm cap: the image controller dispatches them, they execute
    // on buildkitd, and they have never carried an admission identity. They
    // must be RECOGNISED here rather than falling through to the
    // unclassifiable catch-all below, which marks the inventory gate pending
    // and would keep Enforce fail-closed for as long as any image is building.
    if l.get("djinn.app/component")
        .is_some_and(|value| value == "image-build")
        || r.name.starts_with("djinn-build-")
    {
        return Ok(None);
    }
    // SCIP code-graph index Jobs are the same shape of exception as image
    // builds above, for the same reason. `djinn.io/capacity-reserved` is how
    // their CPU is accounted (folded into `protected_mcpu` and subtracted from
    // build capacity by proposal 8ixk); they take NO build lease and carry no
    // admission identity, so there is nothing here to adopt. Falling through to
    // the unclassifiable catch-all would keep Enforce fail-closed for the whole
    // index run — a 4335s budget — during which every worker, reviewer, and
    // planner dispatch is denied `controller_not_admitting`. Observed in
    // production 2026-07-29: one healthy rust-analyzer index blocked the board
    // for its entire duration.
    if l.get("djinn.app/component")
        .is_some_and(|value| value == "scip-index")
        || l.get("djinn.app/scip-index").is_some_and(|v| v == "true")
        || r.name.starts_with("djinn-scip-")
    {
        return Ok(None);
    }
    if (r.name.starts_with("djinn-") || l.keys().any(|k| k.starts_with("djinn.app/")))
        && !r.terminal
        && !r.images.is_empty()
    {
        return Err(format!("{}: unclassifiable build workload", r.name));
    }
    Ok(None)
}
/// The two facts about a listed object that the `Live` branch's task-run
/// completion proof needs: whether it has finished, and which task-run it
/// declares itself to be.
type ListedTaskRun = (bool, Option<String>);

/// Whether the object this Live task-run row recorded exists, declares itself
/// to be that exact task-run, and has already finished.
///
/// This is a POSITIVE identity match, not an absence of contradiction: the
/// object listed under the row's name must carry
/// `djinn.app/task-run-id == row.object_uid`. For a `task_observation` row that
/// uid IS the `task_run_id` (the whole domain is keyed on it — see
/// `live_task_run_build_admission`), and the Job the dispatch created stamps
/// that same id as a label. So this is the task-run's own Job saying it is
/// over, and it plays exactly the role the UID equality plays for warm.
///
/// It deliberately proves nothing about anything else. An object that carries
/// an admission identity of its own has made a claim about which work item it
/// belongs to; if that claim is not this row's, it is a different work item and
/// may not speak for this row — which is what keeps a warm Job's deterministic,
/// REUSED name from letting a predecessor terminalize a live generation.
///
/// Without this, a task-run whose Job has finished but has not yet been deleted
/// by `ttlSecondsAfterFinished` has no exit for a full hour: the identity-keyed
/// lookup above misses (a task-run Job carries no admission labels, so
/// `classify` resolves it to the task-RUN id rather than the row's task id),
/// and the absence proof cannot be made while the object is still listed. That
/// hour is held capacity whenever the ordinary terminal callback is gone —
/// which is every in-flight run across a coordinator restart, because the
/// successor holds no permit binding for the predecessor's generation.
fn finished_as_its_own_task_run(
    row: &AdmissionJournalRow,
    listed: &HashMap<String, ListedTaskRun>,
) -> bool {
    if row.key.domain != AdmissionDomain::TaskObservation {
        return false;
    }
    let Some(task_run_id) = row.object_uid.as_deref() else {
        return false;
    };
    listed
        .get(&row.object_name)
        .is_some_and(|(terminal, declared)| *terminal && declared.as_deref() == Some(task_run_id))
}

/// Whether a reconciliation pass may WRITE the durable admission journal.
///
/// Reconciliation is two different jobs wearing one name. Reading Kubernetes,
/// classifying what it found, and re-deriving this process's own in-memory
/// readiness gates from the journal is safe on any pod: it is all reads, and
/// every gate it sets is process-local. Retiring rows and adopting objects is
/// not — those are writes to the shared durable ledger, and the single-active
/// topology gate exists precisely to make sure only one process performs them.
///
/// The two were fused, so [`BuildAdmissionReconciler::reconcile`] was a
/// mutating pass that `AppState::initialize` ran on **every** pod, standbys
/// included, before leadership was even contested. See the "Why leader-only"
/// section of `djinn_server::build_admission_reconcile` for the invariant that
/// contradicts, and [`BuildAdmissionReconciler::is_reclaimable`] for why it
/// mattered: for a `task_observation` row the LIST and GET clauses are
/// structurally vacuous, so the creator-epoch fence is the *only* thing
/// standing between a settled row and retirement — and a standby passes that
/// fence trivially, because the leader's epoch is not its own. A standby could
/// therefore retire the admission row of a task-run that was still running on
/// the leader, which under-counts durable occupancy: fail-OPEN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileScope {
    /// Read Kubernetes, read the journal, and re-derive this process's own
    /// readiness gates. Writes nothing durable. Safe on any pod, and
    /// deliberately still fail-CLOSED: an unusable listing or an unreadable
    /// journal leaves `InventoryPending` exactly as it would in [`Self::Mutate`].
    Observe,
    /// Everything [`Self::Observe`] does, plus the journal writes — adoption of
    /// live objects and retirement of rows whose objects are provably gone.
    /// Only legitimate on the process that holds the coordinator advisory lock.
    Mutate,
}

impl ReconcileScope {
    /// Whether this pass is allowed to write the durable admission journal.
    fn may_write(self) -> bool {
        matches!(self, Self::Mutate)
    }
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
    ///   finished this create is gone. This clause looks redundant against the
    ///   settle window below and is NOT: the two clauses below are
    ///   *structurally vacuous* for `task_observation` rows. Only warm Jobs are
    ///   stamped with an admission identity (`stamp_admission_identity` is
    ///   called from the warm path alone), a task-run Job is named
    ///   `djinn-taskrun-{task_run_id}` while its journal row records
    ///   `object_name = task-run-{task_id}-{reopen}`, and `classify` maps a
    ///   real task-run Job to a work id that is the task-RUN id. So the LIST
    ///   never contains the row's `object_name` and the GET always answers
    ///   `Absent` — even while the task-run is running happily. For those rows
    ///   the creator epoch is the only evidence that no live work depends on
    ///   the row, and dropping it would retire the admission row of every
    ///   task-run whose POST→session-started gap exceeds the settle window (a
    ///   pending pod or a slow image pull), releasing capacity for work that is
    ///   still running. The 2026-07-29 board halt was NOT caused by this fence:
    ///   it was caused by `seed_from_recovery` ARMING `CreateUnknownHealth`
    ///   from rows this fence is right to refuse. See the arming comment there;
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

    /// Retire one pre-Live occupying row whose Kubernetes evidence is
    /// conclusive: either its object is provably absent, or its object exists
    /// and has already finished. Both are proofs that no lifecycle callback is
    /// coming, and `reclaim_absent_object` is the only journal primitive that
    /// can terminalize a pre-Live row (`mark_terminal` accepts a create whose
    /// UID was never observed, but a pre-Live row that was superseded by a
    /// later generation is rejected by the latest-generation fence every
    /// lifecycle mutation carries).
    ///
    /// The write is fenced by a compare-and-set on the full observed identity,
    /// so anything that changed between the proof and the write yields
    /// `Fenced` and writes nothing.
    async fn retire_pre_live_row(&self, out: &mut InventoryReport, row: &AdmissionJournalRow) {
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
            Err(error) => self.record_reclaim_failure(out, row, &error),
        }
    }

    /// Classify one failed retirement of a single occupying row.
    ///
    /// A rejected transition is a decision about that one row: its own identity
    /// no longer permits the write. Costing all 58 rows for one of them is what
    /// made arming Enforce depend on the namespace's *history* rather than its
    /// current state, so it is counted, named within a bound, and the sweep
    /// continues. Anything else — a connection loss, a serialization failure —
    /// says nothing about the row and everything about the journal, so it stays
    /// a pass-level blocker and marks the journal unhealthy.
    fn record_reclaim_failure(
        &self,
        out: &mut InventoryReport,
        row: &AdmissionJournalRow,
        error: &djinn_db::Error,
    ) {
        if matches!(error, djinn_db::Error::InvalidTransition(_)) {
            out.reclaim_failure(row, &error.to_string());
            tracing::warn!(
                work_id = %row.key.work_id,
                generation = row.key.generation,
                object = %row.object_name,
                %error,
                "build_admission: one occupying row could not be retired; reclamation continues"
            );
            return;
        }
        self.controller.mark_journal_unhealthy();
        out.blockers.push(error.to_string());
    }

    /// Classify one failed adoption of a single live Kubernetes object.
    ///
    /// Symmetric with [`Self::record_reclaim_failure`], and for the same
    /// reason. `adopt_live` returns `InvalidTransition("inventory identity
    /// collision")` for a routine identity mismatch — an object whose
    /// admission labels resolve to a journal key that already holds a
    /// different object name or UID. That is a decision about one object.
    /// Treating it as a pass-level blocker cost the whole pass: it marked the
    /// journal unhealthy (failing Enforce closed on `JournalUnhealthy`) and,
    /// because the tail seed used to run only when `blockers` was empty, it
    /// also froze `journal_recovered`, `journal_healthy`,
    /// `create_unknown_pending` and `over_cap` at whatever they last were —
    /// four gates held hostage by one mislabelled object, indefinitely.
    fn record_adopt_failure(
        &self,
        out: &mut InventoryReport,
        workload: &ClassifiedWorkload,
        error: &djinn_db::Error,
    ) {
        if matches!(error, djinn_db::Error::InvalidTransition(_)) {
            out.row_failure(
                &format!(
                    "{}:{}:{}",
                    workload.key.work_id, workload.key.generation, workload.object.name
                ),
                &error.to_string(),
            );
            tracing::warn!(
                work_id = %workload.key.work_id,
                generation = workload.key.generation,
                object = %workload.object.name,
                %error,
                "build_admission: one live object could not be adopted; the pass continues"
            );
            return;
        }
        self.controller.mark_journal_unhealthy();
        out.blockers.push(error.to_string());
    }

    /// A full, mutating reconciliation pass. Leader-only — see
    /// [`ReconcileScope`].
    pub async fn reconcile(&self) -> InventoryReport {
        self.reconcile_with(ReconcileScope::Mutate).await
    }

    /// A read-only pass: the same evidence gathering and the same process-local
    /// readiness gates, with every journal write suppressed. This is what a pod
    /// that has not confirmed the single-active topology gate runs.
    pub async fn observe(&self) -> InventoryReport {
        self.reconcile_with(ReconcileScope::Observe).await
    }

    pub async fn reconcile_with(&self, scope: ReconcileScope) -> InventoryReport {
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
        // The same listing indexed by name. See `finished_as_its_own_task_run`.
        let listed_task_runs: HashMap<String, ListedTaskRun> = records
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    (r.terminal, r.labels.get(LABEL_TASK_RUN_ID).cloned()),
                )
            })
            .collect();
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
        // Adoption WRITES the journal — it inserts a row for an orphan object
        // that has none, and moves an existing pre-Live row to `Live` — so an
        // observe-only pass skips it. Note that it does NOT restamp an existing
        // row's `creator_server_epoch`: `update_state` never touches that
        // column, so the `creator_server_epoch` supplied here is consumed only
        // by the INSERT arm, where there is no predecessor value to preserve.
        // The pre-Live reclamation fence is therefore never transferred by
        // adoption; see `adopt_live` and its tests.
        if scope.may_write() {
            for c in &cs {
                if c.object.terminal {
                    // Adopting a finished object into `Live` would be a lie. The
                    // row loop below retires the pre-Live row this object belongs
                    // to instead — see the `finished` branch there.
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
                    Err(e) => self.record_adopt_failure(&mut out, c, &e),
                }
            }
        }
        if !scope.may_write() {
            // Everything from here to the tail seed exists to justify and
            // perform a retirement, so an observe-only pass has nothing left to
            // do but re-derive its own gates from the journal — which the tail
            // seed below does, read-only. Skipping the settlement listing and
            // the per-row absence probes also keeps a standby from spending
            // API-server GETs on evidence it may not act on.
            return self.seed_and_publish(out, scope).await;
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
                // Two different proofs, and they need two different writes.
                //
                // A *completed* object still exists, so its lifecycle callback
                // is the ordinary one and `mark_terminal`'s latest-generation
                // fence is meaningful. A *vanished* object has no lifecycle
                // left to run, and requiring the row to be the latest
                // generation there is precisely backwards: a Live row that a
                // later generation has already superseded is the population
                // most in need of retiring, and `mark_terminal` rejects exactly
                // it with `stale admission generation`. That rejection is what
                // left 58 superseded Live rows occupying the cap while every
                // reconciliation pass reported them as blockers.
                let completed = by.get(&id).is_some_and(|c| {
                    c.object.terminal && c.object.uid.as_deref() == row.object_uid.as_deref()
                }) || finished_as_its_own_task_run(row, &listed_task_runs);
                // Absence is proven the same way it always was, plus the LIST
                // fence a pre-Live row already gets: the authoritative listing
                // holds no object under this name, and a direct GET — which
                // answers `NotFound` only on an authoritative `Ok(None)`, never
                // on a transport failure — agrees. A Live row recorded a UID, so
                // that GET is the same probe `presence` would make; asking twice
                // would only add a second chance for a transient `Uncertain` to
                // discard a valid proof.
                let absent = !completed
                    && !listed_names.contains(&row.object_name)
                    && match row.object_uid.as_deref() {
                        Some(uid) => {
                            self.inventory
                                .get_uid(WorkloadObjectKind::Job, &row.object_name, uid)
                                .await
                                == UidGetResult::NotFound
                        }
                        None => false,
                    };
                if !completed && !absent {
                    continue;
                }
                out.stale += 1;
                let outcome = if absent {
                    // Generation-agnostic, fenced on the full observed identity.
                    match self
                        .controller
                        .journal()
                        .reclaim_absent_object(&ReclaimAbsentInput {
                            key: row.key.clone(),
                            observed_state: row.state,
                            observed_creator_server_epoch: row.creator_server_epoch.clone(),
                            observed_object_name: row.object_name.clone(),
                            observed_object_uid: row.object_uid.clone(),
                        })
                        .await
                    {
                        Ok(ReclaimAbsentOutcome::Reclaimed(_)) => Ok(true),
                        Ok(ReclaimAbsentOutcome::AlreadyTerminal(_)) => Ok(false),
                        Ok(ReclaimAbsentOutcome::Fenced { reason }) => {
                            out.fenced += 1;
                            tracing::warn!(
                                work_id = %row.key.work_id,
                                generation = row.key.generation,
                                object = %row.object_name,
                                %reason,
                                "build_admission: refused to retire a Live admission row that \
                                 changed after its absence proof"
                            );
                            Ok(false)
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    self.controller
                        .journal()
                        .mark_terminal(&TerminalAdmissionInput {
                            key: row.key.clone(),
                            object_uid: row.object_uid.clone(),
                        })
                        .await
                        .map(|_| true)
                };
                match outcome {
                    Ok(true) => {
                        out.released += 1;
                        self.controller.release_notifier().notify_one();
                    }
                    Ok(false) => {}
                    Err(error) => self.record_reclaim_failure(&mut out, row, &error),
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
            let classified = by.get(&id).copied();
            // The object EXISTS and has already FINISHED, under this row's own
            // admission identity and its own recorded name. That row was
            // previously in a hole with no exit at all: adoption skips a
            // terminal object (adopting a finished Job into `Live` would be a
            // lie), reclamation refuses it (`classified.is_some()` — the
            // object is not absent, so the absence proof cannot be made), and
            // the `Live` branch's completion handling never applies because
            // the row is not `Live`. So the create landed, the workload ran,
            // the workload finished, and the row occupied capacity forever
            // while every pass reported `stale:0`.
            //
            // A finished object is a strictly stronger proof than absence: the
            // work is over, and no lifecycle callback is coming for a row that
            // never went Live. Retire it.
            let finished =
                classified.is_some_and(|c| c.object.terminal && c.object.name == row.object_name);
            if finished {
                self.retire_pre_live_row(&mut out, row).await;
                continue;
            }
            if !self
                .is_reclaimable(row, &classified, &listed_names, &settled)
                .await
            {
                continue;
            }
            self.retire_pre_live_row(&mut out, row).await;
        }
        self.seed_and_publish(out, scope).await
    }

    /// The read-only tail of every pass, whatever its scope.
    ///
    /// Split out so an observe-only pass reaches it too: re-deriving this
    /// process's own readiness gates from the journal is the part a standby
    /// genuinely needs, and it is all reads. A standby that skipped it would
    /// carry stale gates into its own promotion.
    async fn seed_and_publish(
        &self,
        mut out: InventoryReport,
        scope: ReconcileScope,
    ) -> InventoryReport {
        // The tail seed re-derives `journal_recovered`, `journal_healthy`,
        // `create_unknown_pending` and `over_cap` from the journal. It is the
        // ONLY in-process re-derivation of those four gates, and it only
        // READS: whatever this pass could or could not do to Kubernetes, the
        // journal's current contents are still the journal's current contents.
        //
        // It used to run only `if out.blockers.is_empty()`, which meant any
        // standing blocker — one unclassifiable Job, one duplicate identity,
        // one adoption collision — froze all four gates at their last values
        // for as long as the blocker stood. A gate that had latched closed
        // could never re-open, because the only code that could re-open it was
        // gated on the condition being absent. Only `mark_inventory_ready`
        // belongs behind `blockers`: that gate is a statement about THIS
        // pass's Kubernetes evidence, and it is the one that must stay
        // fail-closed when the evidence is incomplete.
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
        } else if out.blockers.is_empty() {
            self.controller.mark_inventory_ready();
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
        //
        // Only a mutating pass may publish it. An observe-only pass never looks
        // for stale rows at all, so its zeros mean "did not measure" rather
        // than "measured zero", and a gauge that reports the second when it
        // means the first is exactly how a wedge stays invisible.
        if scope.may_write() {
            self.controller.publish_reconciliation(
                out.stale,
                out.released + out.reclaimed,
                out.fenced,
            );
        }
        out
    }
}
