//! Broad, data-only Kubernetes inventory used by coordinator admission recovery.
use async_trait::async_trait;
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{Api, api::ListParams};
use std::collections::BTreeMap;

pub const LABEL_ADMISSION_DOMAIN: &str = "djinn.app/admission-domain";
pub const LABEL_ADMISSION_WORK_ID: &str = "djinn.app/admission-work-id";
pub const LABEL_ADMISSION_GENERATION: &str = "djinn.app/admission-generation";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadObjectKind {
    Job,
    Pod,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadRecord {
    pub kind: WorkloadObjectKind,
    pub name: String,
    pub uid: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub terminal: bool,
    pub images: Vec<String>,
    pub commands: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UidGetResult {
    Present,
    NotFound,
    Uncertain,
}

#[async_trait]
pub trait WorkloadInventory: Send + Sync {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String>;
    async fn get_uid(&self, kind: WorkloadObjectKind, name: &str, uid: &str) -> UidGetResult;
}

pub struct KubeWorkloadInventory {
    client: crate::KubeClient,
    namespace: String,
}
impl KubeWorkloadInventory {
    pub fn new(client: crate::KubeClient, namespace: impl Into<String>) -> Self {
        Self {
            client,
            namespace: namespace.into(),
        }
    }
}

fn containers(spec: Option<&k8s_openapi::api::core::v1::PodSpec>) -> (Vec<String>, Vec<String>) {
    let mut images = Vec::new();
    let mut commands = Vec::new();
    if let Some(spec) = spec {
        for c in &spec.containers {
            if let Some(i) = &c.image {
                images.push(i.clone());
            }
            commands.extend(c.command.clone().unwrap_or_default());
            commands.extend(c.args.clone().unwrap_or_default());
        }
    }
    (images, commands)
}
fn job_record(j: Job) -> Option<WorkloadRecord> {
    let (images, commands) = containers(j.spec.as_ref().and_then(|s| s.template.spec.as_ref()));
    let terminal = j
        .status
        .as_ref()
        .is_some_and(|s| s.succeeded.unwrap_or(0) > 0 || s.failed.unwrap_or(0) > 0);
    Some(WorkloadRecord {
        kind: WorkloadObjectKind::Job,
        name: j.metadata.name?,
        uid: j.metadata.uid,
        labels: j.metadata.labels.unwrap_or_default(),
        terminal,
        images,
        commands,
    })
}
#[async_trait]
impl WorkloadInventory for KubeWorkloadInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        // Child Pods inherit the Job's admission labels. Treating both as
        // independent workloads duplicates task identities and double-counts
        // warm jobs, while the Job is the UID-fenced lifecycle object.
        let jobs = jobs
            .list(&ListParams::default())
            .await
            .map_err(|e| e.to_string())?;
        Ok(jobs.items.into_iter().filter_map(job_record).collect())
    }
    async fn get_uid(&self, kind: WorkloadObjectKind, name: &str, uid: &str) -> UidGetResult {
        let result = match kind {
            WorkloadObjectKind::Job => Api::<Job>::namespaced(self.client.clone(), &self.namespace)
                .get_opt(name)
                .await
                .map(|v| v.and_then(|o| o.metadata.uid)),
            WorkloadObjectKind::Pod => Api::<Pod>::namespaced(self.client.clone(), &self.namespace)
                .get_opt(name)
                .await
                .map(|v| v.and_then(|o| o.metadata.uid)),
        };
        match result {
            Ok(Some(found)) if found == uid => UidGetResult::Present,
            Ok(None) => UidGetResult::NotFound,
            _ => UidGetResult::Uncertain,
        }
    }
}

pub fn has_canonical_warm_signature(r: &WorkloadRecord) -> bool {
    r.name.starts_with("djinn-warm-")
        && !r.images.is_empty()
        && r.commands
            .iter()
            .any(|c| c.contains(crate::warm_job::WARM_COMMAND_BIN) && c.contains("warm-graph"))
}
