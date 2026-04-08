use axum::body::Body;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

pub(super) const CONTRACT_PROJECT_PATH: &str = "/home/fernando/git/djinnos/djinn";

fn parse_sse_json_events(body: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
            continue;
        }

        if line.is_empty() && !data_lines.is_empty() {
            let payload = data_lines.join("\n").trim().to_string();
            if !payload.is_empty()
                && let Ok(value) = serde_json::from_str::<Value>(&payload)
            {
                events.push(value);
            }
            data_lines.clear();
        }
    }

    if !data_lines.is_empty() {
        let payload = data_lines.join("\n").trim().to_string();
        if !payload.is_empty()
            && let Ok(value) = serde_json::from_str::<Value>(&payload)
        {
            events.push(value);
        }
    }

    events
}

pub(super) async fn mcp_jsonrpc(
    app: &axum::Router,
    session_id: &str,
    id: i64,
    method: &str,
    params: Value,
) -> Value {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8(body.to_vec()).expect("response body should be utf-8");

    if let Ok(single) = serde_json::from_str::<Value>(&raw)
        && single.get("id") == Some(&Value::from(id))
    {
        return single;
    }

    parse_sse_json_events(&raw)
        .into_iter()
        .find(|event| event.get("id") == Some(&Value::from(id)))
        .expect("missing JSON-RPC event with requested id")
}

pub(super) fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();

            let mut out = serde_json::Map::new();
            for key in keys {
                if let Some(child) = map.get(&key) {
                    out.insert(key, canonicalize_json(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        _ => value.clone(),
    }
}
