use std::time::{Duration, SystemTime};

use djinn_provider::{
    ProviderAdmissionPolicyV1, ProviderApiKeyNormalizerV1, ProviderDiscoveryOwnershipV1,
    ProviderObservationIgnoreReasonV1, ProviderReceiptTimeV1, ProviderUsageObservationV1,
};

fn headers(epoch: &'static str, request: &'static str) -> [(&'static str, &'static str); 5] {
    [
        ("x-ratelimit-remaining-requests", request),
        ("x-ratelimit-remaining-input-tokens", "8"),
        ("x-ratelimit-remaining-output-tokens", "4"),
        ("x-ratelimit-remaining-tokens", "12"),
        ("x-ratelimit-reset", epoch),
    ]
}

fn receipt() -> ProviderReceiptTimeV1 {
    ProviderReceiptTimeV1 {
        wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        monotonic_ms: 77,
    }
}

#[test]
fn normalizer_reconciles_usage_and_rejects_out_of_order_capacity_growth() {
    let mut normalizer = ProviderApiKeyNormalizerV1::new(ProviderAdmissionPolicyV1::Proactive);
    assert_eq!(
        normalizer.discovery_ownership(),
        ProviderDiscoveryOwnershipV1::DiscoveryRequired
    );
    assert_eq!(
        normalizer.claim_discovery(1),
        ProviderDiscoveryOwnershipV1::DiscoveryOwned {
            request_sequence: 1
        }
    );
    let first = normalizer.observe(
        1,
        &headers("10", "2"),
        ProviderUsageObservationV1 {
            input_units: Some(3),
            output_units: Some(5),
            combined_units: Some(8),
        },
        receipt(),
    );
    assert_eq!(first.authoritative_usage.unwrap().request_units, 1);
    assert_eq!(first.available_capacity.unwrap().request_units, 2);
    assert_eq!(first.discovery, ProviderDiscoveryOwnershipV1::Known);

    let stale_growth = normalizer.observe(
        0,
        &headers("10", "99"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(
        stale_growth.ignored,
        Some(ProviderObservationIgnoreReasonV1::Stale)
    );
    assert_eq!(stale_growth.available_capacity, None);
    // An older response may lower capacity, but it must not move the sequence
    // watermark backwards or permit its other fields to grow capacity.
    let stale_decrease = normalizer.observe(
        0,
        &headers("10", "1"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(stale_decrease.ignored, None);
    assert_eq!(stale_decrease.available_capacity.unwrap().request_units, 1);
    let decreased = normalizer.observe(
        2,
        &headers("10", "1"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(decreased.available_capacity.unwrap().request_units, 1);
}

#[test]
fn reset_bad_sets_retry_and_gemini_reactive_behavior_are_deterministic() {
    let mut normalizer = ProviderApiKeyNormalizerV1::new(ProviderAdmissionPolicyV1::Proactive);
    normalizer.observe(
        4,
        &headers("10", "2"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    let reset = normalizer.observe(
        5,
        &headers("11", "9"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(reset.reset_epoch, Some(11));
    let regressing = normalizer.observe(
        6,
        &headers("9", "20"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(
        regressing.ignored,
        Some(ProviderObservationIgnoreReasonV1::Regressing)
    );
    let incomplete = normalizer.observe(
        7,
        &[("x-ratelimit-remaining-requests", "1")],
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(
        incomplete.ignored,
        Some(ProviderObservationIgnoreReasonV1::Incomplete)
    );
    let impossible = normalizer.observe(
        8,
        &headers("11", "-1"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(
        impossible.ignored,
        Some(ProviderObservationIgnoreReasonV1::Impossible)
    );
    let malformed = normalizer.observe(
        9,
        &headers("not-an-epoch", "1"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(
        malformed.ignored,
        Some(ProviderObservationIgnoreReasonV1::Malformed)
    );
    assert_eq!(malformed.diagnostics.malformed, 1);

    let delta = normalizer.observe(
        10,
        &[("retry-after", "2")],
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(delta.retry_after_deadline_monotonic_ms, Some(2_077));
    let date = normalizer.observe(
        11,
        &[("retry-after", "Thu, 01 Jan 1970 00:16:42 GMT")],
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(date.retry_after_deadline_monotonic_ms, Some(2_077));
    let overflow = normalizer.observe(
        12,
        &[("retry-after", "18446744073709551615")],
        ProviderUsageObservationV1::default(),
        ProviderReceiptTimeV1 {
            monotonic_ms: u64::MAX,
            ..receipt()
        },
    );
    assert_eq!(overflow.retry_after_deadline_monotonic_ms, None);
    assert_eq!(
        overflow.ignored,
        Some(ProviderObservationIgnoreReasonV1::Impossible)
    );
    assert_eq!(overflow.diagnostics.impossible, 2);
    let malformed_retry_after = normalizer.observe(
        13,
        &[("retry-after", "banana")],
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(
        malformed_retry_after.ignored,
        Some(ProviderObservationIgnoreReasonV1::Malformed)
    );
    assert_eq!(malformed_retry_after.diagnostics.malformed, 2);

    let mut gemini =
        ProviderApiKeyNormalizerV1::new(ProviderAdmissionPolicyV1::ReactiveOnlyTarget1);
    let observation = gemini.observe(
        1,
        &headers("10", "99"),
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(observation.available_capacity, None);
    assert_eq!(
        observation.discovery,
        ProviderDiscoveryOwnershipV1::DiscoveryRequired
    );
    let wait = gemini.observe(
        2,
        &[("retry-after", "1")],
        ProviderUsageObservationV1::default(),
        receipt(),
    );
    assert_eq!(wait.retry_after_deadline_monotonic_ms, Some(1_077));
}
