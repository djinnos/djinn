use super::types::RecallTraceParams;
#[test]
fn recall_trace_params_deserialize_filters() {
    let p:RecallTraceParams=serde_json::from_value(serde_json::json!({"mode":"list","project_id":"p","outcome":"skipped","skipped_reason":"not_top_k","limit":10,"offset":2})).unwrap();
    assert_eq!(p.project_id.as_deref(), Some("p"));
    assert_eq!(p.offset, Some(2));
}
