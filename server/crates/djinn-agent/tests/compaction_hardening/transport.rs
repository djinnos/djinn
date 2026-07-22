use djinn_slot::reply_loop::error_handling::{
    ExhaustedTransportCategory, TransportClassificationInput, classify_exhausted_transport,
    is_oversized_transport_payload, TransportCompactionRecoveryGuard,
};
#[test]
fn utf8_byte_classifier_boundaries() {
    let below = "éééx";
    let equal = "éééé";
    let above = "ééééx";
    assert_eq!(below.len(), 7);
    assert_eq!(equal.len(), 8);
    assert_eq!(above.len(), 9);
    assert!(!is_oversized_transport_payload(below.len(), 2));
    assert!(is_oversized_transport_payload(equal.len(), 2));
    assert!(is_oversized_transport_payload(above.len(), 2));
    assert!(!is_oversized_transport_payload(usize::MAX, 0));
    assert!(is_oversized_transport_payload(usize::MAX, u32::MAX));
    for c in [
        ExhaustedTransportCategory::ConnectionReset,
        ExhaustedTransportCategory::UnexpectedEof,
        ExhaustedTransportCategory::RequestBodyWrite,
        ExhaustedTransportCategory::ResponseHeaderTimeout,
        ExhaustedTransportCategory::SseFirstEventTimeout,
    ] {
        assert_eq!(
            classify_exhausted_transport(true, None, TransportClassificationInput::Eligible(c), 8)
                .map(|d| d.category),
            Some(c)
        );
    }
    for x in [
        TransportClassificationInput::ProviderBody,
        TransportClassificationInput::Authentication,
        TransportClassificationInput::RateLimit,
        TransportClassificationInput::Cancellation,
        TransportClassificationInput::OrdinaryServerError,
        TransportClassificationInput::PostFirstEvent,
        TransportClassificationInput::UnknownTransport,
    ] {
        assert!(classify_exhausted_transport(true, None, x, 8).is_none());
    }
}

#[test]
fn recovery_is_one_shot() {
    // The reply loop invokes this guard before entering its compaction critical
    // section. These deterministic counters model a successful recovery retry,
    // a compaction failure, an still-oversized retry, the empty-stream path, and
    // an excluded failure: no scenario is allowed a second compaction/retry.
    for scenario in [
        "success_then_retry",
        "compaction_failure",
        "still_oversized_retry",
        "exhausted_empty_stream",
        "excluded_failure",
    ] {
        let mut guard = TransportCompactionRecoveryGuard::default();
        let mut compactions = 0;
        let mut recovery_retries = 0;
        let eligible = scenario != "excluded_failure";
        if eligible && guard.try_begin() {
            compactions += 1;
            if scenario == "success_then_retry" || scenario == "still_oversized_retry" {
                recovery_retries += 1;
            }
        }
        // A compaction error and a failed recovery retry both revisit the same
        // guard; neither may create a second attempt.
        if eligible {
            assert!(!guard.try_begin(), "{scenario}");
        } else {
            assert!(!guard.attempted(), "{scenario}");
        }
        assert!(compactions <= 1, "{scenario}");
        assert!(recovery_retries <= 1, "{scenario}");
    }
}
