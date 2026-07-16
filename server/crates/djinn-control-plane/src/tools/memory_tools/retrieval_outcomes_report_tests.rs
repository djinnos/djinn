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
