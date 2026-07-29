use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::{LAUNCHER_CGROUP_ROOT, TASK_RUN_CGROUP_RUNTIME_CLASS};
use uuid::Uuid;

#[test]
fn activated_launcher_uses_delegated_runtime_class_without_cgroup_volume() {
    let config = KubernetesConfig::for_testing();
    let job = build_task_run_job(
        &config,
        &Uuid::now_v7(),
        "project",
        "secret",
        "image",
        &[],
        None,
        false,
        None,
    );
    let pod = job.spec.unwrap().template.spec.unwrap();
    assert_eq!(
        pod.runtime_class_name.as_deref(),
        Some(TASK_RUN_CGROUP_RUNTIME_CLASS)
    );
    assert!(
        pod.volumes
            .iter()
            .flatten()
            .all(|volume| volume.host_path.is_none())
    );
    assert!(
        pod.containers
            .iter()
            .chain(pod.init_containers.iter().flatten())
            .flat_map(|container| container.volume_mounts.iter().flatten())
            .all(|mount| mount.mount_path != LAUNCHER_CGROUP_ROOT)
    );
}
