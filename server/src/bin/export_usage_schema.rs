#![allow(clippy::disallowed_methods)]
//! Export the JSON Schema for `/api/admin/usage` request/response DTOs.
//!
//! Usage:
//!   cargo run -p djinn-server --bin export-usage-schema > server/schemas/usage-analytics.schema.json
//!
//! This binary is a build tool used by the `ui/scripts/generate-usage-types.ts`
//! pipeline to produce the checked-in TypeScript contract artifact
//! `ui/src/api/generated/usage-analytics.gen.ts`.

use djinn_server::server::usage_analytics::{usage_query_json_schema, usage_response_json_schema};

fn main() {
    let combined = serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "UsageAnalyticsApi",
        "description": "Combined JSON Schema for the /api/admin/usage endpoint query parameters and response body, derived from Rust DTOs via schemars.",
        "definitions": {
            "UsageQuery": usage_query_json_schema(),
            "UsageResponse": usage_response_json_schema(),
        }
    });
    let output = serde_json::to_string_pretty(&combined).expect("valid JSON");
    println!("{output}");
}
