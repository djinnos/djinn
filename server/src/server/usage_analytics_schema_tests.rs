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

/// Write the full UsageResponse + UsageQuery JSON Schema to
/// `server/schemas/usage-analytics.schema.json` so the TypeScript generation
/// pipeline (`ui/scripts/generate-usage-types.ts`) can consume it.
///
/// Run with:
///   cargo test -p djinn-server usage_analytics_schema_tests::export_schemas_to_file -- --nocapture
#[test]
fn export_schemas_to_file() {
    use std::io::Write;

    let response_schema = schemars::schema_for!(UsageResponse);
    let query_schema = schemars::schema_for!(UsageQuery);

    let combined = serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "UsageAnalyticsApi",
        "description": "Combined JSON Schema for the /api/admin/usage endpoint query parameters and response body, derived from Rust DTOs via schemars.",
        "definitions": {
            "UsageQuery": serde_json::to_value(&query_schema).unwrap(),
            "UsageResponse": serde_json::to_value(&response_schema).unwrap(),
        }
    });

    let json = serde_json::to_string_pretty(&combined).unwrap();

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_path = std::path::Path::new(manifest_dir)
        .join("schemas")
        .join("usage-analytics.schema.json");

    std::fs::create_dir_all(out_path.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(&out_path).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    f.write_all(b"\n").unwrap();

    eprintln!("Wrote schema to {}", out_path.display());
    eprintln!("Schema size: {} bytes", json.len());
}
