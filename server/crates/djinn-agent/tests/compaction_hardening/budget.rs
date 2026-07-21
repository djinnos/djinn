use djinn_provider::provider::TokenUsage;
use djinn_slot::reply_loop::budget::UsageAccountingForTest;

#[test]
fn lifetime_counters_survive_two_compactions() {
    let mut usage = UsageAccountingForTest::default();
    usage.record(&TokenUsage {
        input: 40,
        output: 20,
        context_total: 50,
        ..Default::default()
    });
    // The context-length recovery compacts the first request, but must not
    // erase its contribution to the reply-loop lifetime budget.
    usage.clear_occupancy_after_reactive_compaction();
    assert_eq!(usage.current_context_tokens, 0);
    usage.record(&TokenUsage {
        input: 20,
        output: 5,
        context_total: 60,
        ..Default::default()
    });
    // 85 lifetime tokens reaches the fixed 75% soft threshold, even though
    // the current context belongs only to the retry after reactive compaction.
    assert!(usage.exceeds_soft_lifetime_budget(100, 0.75));
    assert!(!usage.exceeds_hard_lifetime_budget(100, 0.92));

    // The subsequent proactive compaction is a distinct path and likewise
    // clears occupancy only.
    usage.clear_occupancy_after_proactive_compaction();
    assert_eq!(usage.current_context_tokens, 0);
    usage.record(&TokenUsage {
        input: 10,
        output: 10,
        context_total: 30,
        ..Default::default()
    });

    // 105 lifetime tokens crosses the fixed 92% hard threshold after both
    // compaction stages; occupancy has been repopulated from the last call.
    assert_eq!(
        (usage.lifetime_tokens_in, usage.lifetime_tokens_out),
        (70, 35)
    );
    assert!(usage.exceeds_hard_lifetime_budget(100, 0.92));
    assert_eq!(usage.current_context_tokens, 30);
}

#[test]
fn cache_usage_is_occupancy_not_double_billed() {
    let mut usage = UsageAccountingForTest::default();
    usage.record(&TokenUsage {
        input: 100,
        output: 7,
        cache_read: 80,
        cache_write: 10,
        context_total: 100,
        ..Default::default()
    });
    assert_eq!(usage.lifetime_tokens_in, 100);
    assert_eq!(usage.lifetime_tokens_out, 7);
    assert_eq!(
        (usage.lifetime_cache_read, usage.lifetime_cache_write),
        (80, 10)
    );
    assert_eq!(usage.current_context_tokens, 100);
}
