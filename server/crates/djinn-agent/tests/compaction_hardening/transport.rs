use djinn_slot::reply_loop::error_handling::{
    ExhaustedTransportCategory, TransportClassificationInput, classify_exhausted_transport,
    is_oversized_transport_payload,
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
