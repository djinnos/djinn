//! Best-effort capture of the last log lines from a dying K8s worker Pod.
//!
//! Called between `watch_infra_death` resolving and `teardown` deleting the
//! Job, so the Pod may still exist on the apiserver for a brief window.
//!
//! Design constraints:
//! - Short timeout (≤ 10 s) — must never block teardown.
//! - Truncates to the `task_attempts.log_tail` DB bound (~16 KiB).
//! - Returns `None` on any failure — capture is purely diagnostic enrichment.

use std::time::Duration;

use djinn_runtime::InfraDeathLogTailCapture;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams};
use tracing::{debug, warn};

use crate::job::LABEL_TASK_RUN_ID;

/// Maximum bytes of log tail to persist in `task_attempts.log_tail`.
/// Matches `TASK_ATTEMPT_LOG_TAIL_MAX_LEN` in djinn-core.
const LOG_TAIL_MAX_BYTES: usize = 16 * 1024;

/// Maximum number of log lines to request from the apiserver.
const LOG_TAIL_LINE_COUNT: i64 = 200;

/// Timeout for the entire capture operation (pod list + log fetch).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);

/// Try to capture the last log lines from the worker Pod's container after an
/// infra-death has been detected.  Returns `None` on any failure.
///
/// The `namespace` and `client` come from the same `KubernetesRuntime` that
/// owns the Pod.  The `task_run_id` is used to find the Pod via the standard
/// task-run label selector.
pub async fn capture_infra_death_log_tail(
    client: &kube::Client,
    namespace: &str,
    task_run_id: &str,
) -> Option<InfraDeathLogTailCapture> {
    let result =
        tokio::time::timeout(CAPTURE_TIMEOUT, do_capture(client, namespace, task_run_id)).await;

    match result {
        Ok(capture) => capture,
        Err(_elapsed) => {
            warn!(
                task_run_id,
                "infra_death_log_tail: capture timed out after {:?}", CAPTURE_TIMEOUT
            );
            Some(InfraDeathLogTailCapture {
                log_tail: None,
                fetch_error_class: Some("timeout".to_owned()),
                fetch_error_detail: Some(format!(
                    "log-tail capture timed out after {:?}",
                    CAPTURE_TIMEOUT
                )),
            })
        }
    }
}

async fn do_capture(
    client: &kube::Client,
    namespace: &str,
    task_run_id: &str,
) -> Option<InfraDeathLogTailCapture> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let label_selector = format!("{}={}", LABEL_TASK_RUN_ID, task_run_id);

    // 1. Find the Pod.
    let pod_name = match pods
        .list(&ListParams::default().labels(&label_selector))
        .await
    {
        Ok(list) => match list.items.into_iter().next() {
            Some(pod) => {
                let name = pod.metadata.name.clone().unwrap_or_default();
                if name.is_empty() {
                    return Some(InfraDeathLogTailCapture {
                        log_tail: None,
                        fetch_error_class: Some("pod_not_found".to_owned()),
                        fetch_error_detail: Some("Pod found but has no name".to_owned()),
                    });
                }
                name
            }
            None => {
                debug!(
                    task_run_id,
                    "infra_death_log_tail: no Pod found (already GC'd)"
                );
                return Some(InfraDeathLogTailCapture {
                    log_tail: None,
                    fetch_error_class: Some("pod_not_found".to_owned()),
                    fetch_error_detail: Some(
                        "Pod not found by label (likely already GC'd by Job TTL)".to_owned(),
                    ),
                });
            }
        },
        Err(e) => {
            warn!(
                task_run_id,
                error = %e,
                "infra_death_log_tail: pod list failed"
            );
            return Some(InfraDeathLogTailCapture {
                log_tail: None,
                fetch_error_class: Some("pod_list_error".to_owned()),
                fetch_error_detail: Some(format!("Pod list failed: {e}")),
            });
        }
    };

    // 2. Fetch logs from the `worker` container (falls back to first container).
    let log_params = kube::api::LogParams {
        container: Some("worker".to_owned()),
        tail_lines: Some(LOG_TAIL_LINE_COUNT),
        limit_bytes: Some(LOG_TAIL_MAX_BYTES as i64),
        ..Default::default()
    };

    let logs = match pods.logs(&pod_name, &log_params).await {
        Ok(logs) => logs,
        Err(e) => {
            warn!(
                task_run_id,
                pod = %pod_name,
                error = %e,
                "infra_death_log_tail: log fetch failed"
            );
            return Some(InfraDeathLogTailCapture {
                log_tail: None,
                fetch_error_class: Some("log_fetch_error".to_owned()),
                fetch_error_detail: Some(format!("Pod log fetch failed: {e}")),
            });
        }
    };

    if logs.is_empty() {
        debug!(
            task_run_id,
            pod = %pod_name,
            "infra_death_log_tail: pod logs are empty"
        );
        return Some(InfraDeathLogTailCapture {
            log_tail: None,
            fetch_error_class: Some("empty_logs".to_owned()),
            fetch_error_detail: Some("Pod logs are empty".to_owned()),
        });
    }

    // 3. Truncate to the DB bound if the apiserver's limit_bytes wasn't exact.
    let tail = truncate_to_utf8_boundary(&logs, LOG_TAIL_MAX_BYTES);

    debug!(
        task_run_id,
        pod = %pod_name,
        byte_count = tail.len(),
        "infra_death_log_tail: captured successfully"
    );

    Some(InfraDeathLogTailCapture {
        log_tail: Some(tail.to_owned()),
        fetch_error_class: None,
        fetch_error_detail: None,
    })
}

/// Truncate a string slice to at most `max_bytes` bytes, ensuring the cut
/// point falls on a UTF-8 character boundary.
fn truncate_to_utf8_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Find the last valid char boundary at or before max_bytes.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_to_utf8_boundary("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_boundary() {
        assert_eq!(truncate_to_utf8_boundary("hello", 5), "hello");
    }

    #[test]
    fn truncate_ascii() {
        assert_eq!(truncate_to_utf8_boundary("hello world", 5), "hello");
    }

    #[test]
    fn truncate_multibyte_utf8() {
        // "é" is 2 bytes in UTF-8.
        let s = "ééé"; // 6 bytes
        assert_eq!(truncate_to_utf8_boundary(s, 5).len(), 4); // "éé" = 4 bytes
        assert_eq!(truncate_to_utf8_boundary(s, 4).len(), 4); // "éé"
        assert_eq!(truncate_to_utf8_boundary(s, 3).len(), 2); // "é"
    }
}
