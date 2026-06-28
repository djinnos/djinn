use super::*;

#[test]
fn usage_response_json_schema_roundtrips_and_covers_split_fields() {
    let schema = schemars::schema_for!(UsageResponse);
    let value = serde_json::to_value(&schema).unwrap();

    // Top-level fields must be present in the schema.
    let props = value["properties"].as_object().unwrap();
    for field in [
        "kpis",
        "time_series",
        "breakdowns",
        "model_effectiveness",
        "project_model_matrix",
        "generated_at",
        "unpriced_session_count",
    ] {
        assert!(
            props.contains_key(field),
            "schema missing top-level field {field}"
        );
    }

    // KPI schema must contain the split aggregate fields.
    let kpi_defs = value["$defs"]["UsageKpiDto"]["properties"]
        .as_object()
        .unwrap();
    for field in ["actual_spend_usd", "projected_usd", "unpriced_count"] {
        assert!(
            kpi_defs.contains_key(field),
            "schema missing UsageKpiDto field {field}"
        );
    }

    // Breakdown schema must contain split fields.
    let row_defs = value["$defs"]["BreakdownRowDto"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "actual_spend_usd",
        "projected_usd",
        "unpriced_session_count",
    ] {
        assert!(
            row_defs.contains_key(field),
            "schema missing BreakdownRowDto field {field}"
        );
    }

    // SeriesPoint schema must contain split fields.
    let series_defs = value["$defs"]["SeriesPointDto"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "actual_spend_usd",
        "projected_usd",
        "unpriced_session_count",
    ] {
        assert!(
            series_defs.contains_key(field),
            "schema missing SeriesPointDto field {field}"
        );
    }

    // ModelEffectiveness schema must contain split fields.
    let eff_defs = value["$defs"]["ModelEffectivenessDto"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "actual_spend_usd",
        "projected_usd",
        "unpriced_session_count",
    ] {
        assert!(
            eff_defs.contains_key(field),
            "schema missing ModelEffectivenessDto field {field}"
        );
    }

    // ProjectModelCell schema must contain split fields.
    let cell_defs = value["$defs"]["ProjectModelCellDto"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "actual_spend_usd",
        "projected_usd",
        "unpriced_session_count",
    ] {
        assert!(
            cell_defs.contains_key(field),
            "schema missing ProjectModelCellDto field {field}"
        );
    }
}

#[test]
fn usage_response_json_schema_can_be_written_to_file() {
    let schema = schemars::schema_for!(UsageResponse);
    let json = serde_json::to_string_pretty(&schema).unwrap();
    assert!(
        json.contains("UsageResponse"),
        "schema JSON should reference UsageResponse"
    );
    assert!(
        json.contains("actual_spend_usd"),
        "schema JSON should contain actual_spend_usd"
    );
    assert!(
        json.contains("projected_usd"),
        "schema JSON should contain projected_usd"
    );
    assert!(
        json.contains("unpriced_session_count"),
        "schema JSON should contain unpriced_session_count"
    );
    assert!(
        json.contains("unpriced_count"),
        "schema JSON should contain unpriced_count"
    );
}
