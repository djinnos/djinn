use djinn_slot::reply_loop::error_handling::{
    ExhaustedTransportCategory, TransportClassificationInput, TransportCompactionRecoveryGuard,
    classify_exhausted_transport, is_oversized_transport_payload,
};
use std::collections::VecDeque;

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

#[derive(Clone, Copy)]
enum TransportResult {
    EligibleOversized,
    EmptyHttp200,
    Excluded,
    Success,
}

/// Deterministic fake transport plus the compaction seam used by the recovery
/// boundary. It executes the same guard and exact-byte predicate as the loop.
struct FakeReplyLoop {
    transport: VecDeque<TransportResult>,
    provider_calls: usize,
    compaction_calls: usize,
    budget_reentries: usize,
    compaction_succeeds: bool,
    payload_bytes: Option<usize>,
    window_tokens: u32,
    terminal: Option<&'static str>,
}

impl FakeReplyLoop {
    fn run(&mut self) {
        let mut guard = TransportCompactionRecoveryGuard::default();
        while let Some(result) = self.transport.pop_front() {
            self.provider_calls += 1;
            let eligible = match result {
                TransportResult::EligibleOversized | TransportResult::EmptyHttp200 => self
                    .payload_bytes
                    .is_some_and(|bytes| is_oversized_transport_payload(bytes, self.window_tokens)),
                TransportResult::Excluded | TransportResult::Success => false,
            };
            if matches!(result, TransportResult::Success) {
                return;
            }
            if eligible && guard.try_begin() {
                self.compaction_calls += 1;
                if !self.compaction_succeeds {
                    self.terminal = Some("original transport/empty-stream error");
                    return;
                }
                self.budget_reentries += 1;
                continue;
            }
            self.terminal = Some(match result {
                TransportResult::EmptyHttp200 => "original empty-stream error",
                _ => "original transport error",
            });
            return;
        }
    }
}

#[test]
fn recovery_is_one_shot() {
    let cases = [
        (
            "success then retry",
            vec![TransportResult::EligibleOversized, TransportResult::Success],
            true,
            Some(8),
            2,
            2,
            1,
            1,
            None,
        ),
        (
            "compaction failure",
            vec![TransportResult::EligibleOversized],
            false,
            Some(8),
            2,
            1,
            1,
            0,
            Some("original transport/empty-stream error"),
        ),
        (
            "still oversized retry",
            vec![
                TransportResult::EligibleOversized,
                TransportResult::EligibleOversized,
            ],
            true,
            Some(8),
            2,
            2,
            1,
            1,
            Some("original transport error"),
        ),
        (
            "exhausted empty stream",
            vec![TransportResult::EmptyHttp200, TransportResult::Success],
            true,
            Some(8),
            2,
            2,
            1,
            1,
            None,
        ),
        (
            "excluded failure",
            vec![TransportResult::Excluded],
            true,
            Some(8),
            2,
            1,
            0,
            0,
            Some("original transport error"),
        ),
    ];
    for (
        name,
        script,
        compaction_succeeds,
        payload_bytes,
        window_tokens,
        calls,
        compactions,
        budgets,
        terminal,
    ) in cases
    {
        let mut loop_ = FakeReplyLoop {
            transport: script.into(),
            provider_calls: 0,
            compaction_calls: 0,
            budget_reentries: 0,
            compaction_succeeds,
            payload_bytes,
            window_tokens,
            terminal: None,
        };
        loop_.run();
        assert_eq!(loop_.provider_calls, calls, "{name}");
        assert_eq!(loop_.compaction_calls, compactions, "{name}");
        assert_eq!(loop_.budget_reentries, budgets, "{name}");
        assert_eq!(loop_.terminal, terminal, "{name}");
        assert!(loop_.compaction_calls <= 1, "{name}");
    }
}
