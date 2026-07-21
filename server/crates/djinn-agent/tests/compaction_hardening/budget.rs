use djinn_provider::provider::TokenUsage;
use djinn_slot::reply_loop::budget::UsageAccountingForTest;

#[test]
fn lifetime_counters_survive_two_compactions() {
    let mut usage = UsageAccountingForTest::default();
    usage.record(&TokenUsage {
        input: 30,
        output: 20,
        context_total: 50,
        ..Default::default()
    });
    usage.clear_occupancy_after_compaction();
    usage.record(&TokenUsage {
        input: 35,
        output: 25,
        context_total: 60,
        ..Default::default()
    });
    usage.clear_occupancy_after_compaction();
    assert_eq!(
        (usage.lifetime_tokens_in, usage.lifetime_tokens_out),
        (65, 45)
    );
    assert_eq!(usage.current_context_tokens, 0);
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
