use super::*;
use async_trait::async_trait;
use std::collections::HashSet;

struct RecordingAdmission {
    issued: Mutex<HashSet<WarmAdmissionPermit>>,
    requests: Mutex<Vec<WarmAdmissionRequest>>,
    transitions: Mutex<Vec<WarmAdmissionTransition>>,
}

impl RecordingAdmission {
    fn new() -> Self {
        Self {
            issued: Mutex::new(HashSet::new()),
            requests: Mutex::new(Vec::new()),
            transitions: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl WarmAdmission for RecordingAdmission {
    async fn admit(
        &self,
        request: WarmAdmissionRequest,
    ) -> Result<WarmAdmissionPermit, WarmAdmissionError> {
        self.requests.lock().await.push(request);
        let permit = WarmAdmissionPermit::new();
        self.issued.lock().await.insert(permit.clone());
        Ok(permit)
    }

    async fn transition(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        if !self.issued.lock().await.contains(permit) {
            return Err(WarmAdmissionError::UnknownPermit);
        }
        self.transitions.lock().await.push(transition);
        Ok(())
    }
}

#[tokio::test]
async fn admission_request_transitions_and_opaque_permits_are_data_only() {
    let admission = RecordingAdmission::new();
    let request = WarmAdmissionRequest {
        domain: "graph-warm".to_string(),
        work_id: "project-123".to_string(),
        generation: 7,
        object_name: "djinn-warm-project-123-g7".to_string(),
    };

    let permit = admission.admit(request.clone()).await.expect("admitted");
    admission
        .transition(&permit, WarmAdmissionTransition::CreateStarted)
        .await
        .expect("issued permit is recognized");
    admission
        .transition(
            &permit,
            WarmAdmissionTransition::Live {
                uid: "job-uid".to_string(),
            },
        )
        .await
        .expect("issued permit remains recognized");

    assert_eq!(*admission.requests.lock().await, vec![request]);
    assert_eq!(
        *admission.transitions.lock().await,
        vec![
            WarmAdmissionTransition::CreateStarted,
            WarmAdmissionTransition::Live {
                uid: "job-uid".to_string(),
            },
        ]
    );
    assert_eq!(
        format!("{permit:?}"),
        "WarmAdmissionPermit(..)",
        "the permit does not expose controller ledger data"
    );

    let forged = WarmAdmissionPermit::new();
    assert_eq!(
        admission
            .transition(
                &forged,
                WarmAdmissionTransition::Terminal {
                    uid: "job-uid".to_string(),
                },
            )
            .await,
        Err(WarmAdmissionError::UnknownPermit),
        "a controller recognizes only permits it issued"
    );
}

#[test]
fn admission_transition_variants_preserve_uids_and_diagnostics() {
    assert_eq!(
        WarmAdmissionTransition::CreateUnknown {
            diagnostic: "connection reset after POST".to_string(),
        },
        WarmAdmissionTransition::CreateUnknown {
            diagnostic: "connection reset after POST".to_string(),
        }
    );
    assert_eq!(
        WarmAdmissionTransition::DefinitiveFailure {
            diagnostic: "forbidden".to_string(),
        },
        WarmAdmissionTransition::DefinitiveFailure {
            diagnostic: "forbidden".to_string(),
        }
    );
    assert_eq!(
        WarmAdmissionTransition::Terminal {
            uid: "job-uid".to_string(),
        },
        WarmAdmissionTransition::Terminal {
            uid: "job-uid".to_string(),
        }
    );
}

/// A refused warm cycle re-runs the full dispatch attempt: a mirror tip
/// discovery plus an advisory-locked lease transaction. A flat interval turned
/// a permanently closed gate into an unbounded hot loop (thousands of identical
/// refusals per day in production), so the delay must grow and then saturate.
#[test]
fn admission_retry_delay_backs_off_and_saturates() {
    assert_eq!(admission_retry_delay(1), Duration::from_secs(1));
    assert_eq!(admission_retry_delay(2), Duration::from_secs(2));
    assert_eq!(admission_retry_delay(3), Duration::from_secs(4));
    assert_eq!(admission_retry_delay(7), Duration::from_secs(60));

    let mut previous = Duration::ZERO;
    for attempt in 1..=64 {
        let delay = admission_retry_delay(attempt);
        assert!(
            delay >= previous,
            "the backoff must never shrink (attempt {attempt})"
        );
        assert!(
            delay <= Duration::from_secs(60),
            "the backoff must stay bounded (attempt {attempt})"
        );
        previous = delay;
    }
    assert_eq!(
        admission_retry_delay(u32::MAX),
        Duration::from_secs(60),
        "an unbounded refusal streak must not overflow into a shorter delay"
    );
}
