use super::RetrievalOutcomesReportParams;
#[test]
fn retrieval_outcomes_report_dto_requires_explicit_interval() {
    assert!(serde_json::from_value::<RetrievalOutcomesReportParams>(serde_json::json!({"project":"p","start":"2026-07-01T00:00:00Z","end":"2026-07-02T00:00:00Z","timezone":"UTC"})).is_ok());
    assert!(
        serde_json::from_value::<RetrievalOutcomesReportParams>(
            serde_json::json!({"project":"p","start":"x","end":"y","task_id":"no"})
        )
        .is_err()
    );
}

#[test]
fn retrieval_outcomes_report_dispatch_contract_rejects_unsupported_arguments() {
    let request = serde_json::json!({
        "project": "p",
        "start": "2026-07-01T00:00:00Z",
        "end": "2026-07-02T00:00:00Z",
        "timezone": "UTC",
        "task_id": "must-not-be-used-as-a-fallback"
    });
    let error = serde_json::from_value::<RetrievalOutcomesReportParams>(request)
        .err()
        .expect("report DTO must reject task_id rather than silently join through it");
    assert!(error.to_string().contains("unknown field `task_id`"));
}
