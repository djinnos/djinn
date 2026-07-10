use super::*;
use serde_json::json;

fn has_label(s: &str, label: &str) -> bool {
    // Bullet format: "- {label}:". Allow either the colon or end-of-line
    // form so we accept both truncated and full bullets.
    let needle = format!("- {label}:");
    s.contains(&needle) || s.lines().any(|line| line.trim_start_matches("- ") == label)
}

// -- JSON tests --

#[test]
fn empty_input_returns_none() {
    assert_eq!(synopsize("shell", "", 1024), None);
    assert_eq!(synopsize("shell", "   \n\t  ", 1024), None);
}

#[test]
fn zero_budget_returns_none() {
    assert_eq!(synopsize("shell", "{\"a\":1}", 0), None);
}
#[test]
fn json_object_produces_stable_labels() {
    let value = json!({"ok": true, "rows": 3, "users": [{"id": 1, "name": "alice"}, {"id": 2, "name": "bob"}], "config": {"host": "example.com", "port": 443, "tls": true}});
    let raw = serde_json::to_string(&value).unwrap();
    let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
    assert!(has_label(&out, "kind"), "kind missing: {out}");
    assert!(has_label(&out, "root"), "root missing: {out}");
    assert!(has_label(&out, "arrays"), "arrays missing: {out}");
    assert!(
        has_label(&out, "object shape depth 2"),
        "object shape depth 2 missing: {out}"
    );
}
#[test]
fn json_array_produces_stable_labels() {
    let value =
        json!([{"id": 1, "name": "alpha"}, {"id": 2, "name": "beta"}, {"id": 3, "name": "gamma"}]);
    let raw = serde_json::to_string(&value).unwrap();
    let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
    assert!(has_label(&out, "kind"));
    assert!(has_label(&out, "root"));
    assert!(has_label(&out, "arrays"));
    assert!(out.contains("root=3"), "expected root=3: {out}");
}
#[test]
fn scalar_examples_are_bounded() {
    let value = json!({"status": "ok", "code": 200, "flag": true, "name": "alpha"});
    let raw = serde_json::to_string(&value).unwrap();
    let out = synopsize("shell", &raw, 1200).expect("synopsis present");
    assert!(has_label(&out, "scalar examples"));
    let bullet = out
        .lines()
        .find(|l| l.starts_with("- scalar examples"))
        .unwrap();
    for sample in bullet.trim_start_matches("- scalar examples:").split(',') {
        assert!(sample.trim().chars().count() <= MAX_SCALAR_EXAMPLE_CHARS + 1);
    }
}
#[test]
fn suggested_grep_terms_for_status_string() {
    let value = json!({"status": "completed", "result": "ok", "user_id": "u_123"});
    let raw = serde_json::to_string(&value).unwrap();
    let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
    assert!(has_label(&out, "suggested grep terms"));
    let bullet = out
        .lines()
        .find(|l| l.starts_with("- suggested grep terms"))
        .unwrap();
    let terms: Vec<&str> = bullet
        .trim_start_matches("- suggested grep terms:")
        .split(',')
        .map(str::trim)
        .collect();
    assert!(terms.len() <= MAX_GREP_TERMS);
    for t in terms {
        assert!(t.chars().count() <= MAX_GREP_TERM_CHARS);
        assert!(!t.is_empty());
    }
}
#[test]
fn malformed_json_falls_back_to_text() {
    // Starts with `{` so the heuristic accepts it, but the body is
    // syntactically broken. Falls through binary check (not binary),
    // code check (not code), then lands on text.
    let result = synopsize("shell", "{not json", 1024);
    assert!(
        result.is_some(),
        "malformed JSON should fall through to text"
    );
}
#[test]
fn pathological_depth_does_not_panic() {
    let mut s = String::new();
    for _ in 0..10_000 {
        s.push('[');
    }
    s.push('1');
    for _ in 0..10_000 {
        s.push(']');
    }
    let result = synopsize("shell", &s, 4096);
    if let Some(out) = result {
        assert!(out.len() <= 4096);
    }
}
#[test]
fn oversized_valid_json_returns_none_or_text() {
    let inner = "1,".repeat(MAX_PARSE_BYTES / 2 + 1);
    let trimmed_inner = inner.trim_end_matches(',');
    let raw = format!("[{trimmed_inner}]");
    assert!(raw.len() > MAX_PARSE_BYTES);
    let result = synopsize("shell", &raw, 4096);
    if let Some(out) = result {
        assert!(
            !has_label(&out, "root"),
            "oversized JSON must not produce JSON synopsis"
        );
    }
}
#[test]
fn pathological_breadth_over_token_limit_returns_none_or_text() {
    let n = MAX_JSON_TOKENS + 10;
    let raw: String = std::iter::once('[')
        .chain(
            (0..n)
                .flat_map(|i| std::iter::once('1').chain(if i + 1 < n { Some(',') } else { None })),
        )
        .chain(std::iter::once(']'))
        .collect();
    assert!(raw.len() < MAX_PARSE_BYTES);
    let result = synopsize("shell", &raw, 4096);
    if let Some(out) = result {
        assert!(out.len() <= 4096);
    }
}
#[test]
fn pathological_breadth_does_not_panic() {
    let mut map = serde_json::Map::with_capacity(50_000);
    for i in 0..50_000 {
        map.insert(format!("k{i}"), serde_json::Value::Bool(i % 2 == 0));
    }
    let raw = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
    let result = synopsize("shell", &raw, 4096);
    if let Some(s) = result {
        assert!(s.len() <= 4096);
    }
}
#[test]
fn pathological_breadth_just_under_limit_returns_bounded_synopsis() {
    let n = 5_000;
    let mut map = serde_json::Map::with_capacity(n);
    for i in 0..n {
        map.insert(format!("k{i}"), serde_json::Value::Number(i.into()));
    }
    let raw = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
    let out = synopsize("shell", &raw, 4096).expect("synopsis for large-but-valid JSON");
    assert!(out.len() <= 4096);
    assert!(has_label(&out, "kind"));
    assert!(has_label(&out, "root"));
}
#[test]
fn budget_enforcement_caps_total_length() {
    let value = json!({"alpha": "x".repeat(200), "beta": "y".repeat(200), "gamma": "z".repeat(200), "delta": [1,2,3,4,5,6,7,8], "epsilon": {"k1": 1, "k2": 2, "k3": 3, "k4": 4}});
    let raw = serde_json::to_string(&value).unwrap();
    let out = synopsize("shell", &raw, 80).expect("synopsis present");
    assert!(out.len() <= 80);
    assert!(has_label(&out, "kind"));
    assert!(has_label(&out, "root"));
}
#[test]
fn budget_zero_is_safe() {
    assert_eq!(
        synopsize(
            "shell",
            &serde_json::to_string(&json!({"a": 1})).unwrap(),
            0
        ),
        None
    );
}
#[test]
fn empty_object_json_is_supported() {
    let out = synopsize("shell", "{}", 1024).unwrap();
    assert!(out.contains("empty object"));
}
#[test]
fn empty_array_json_is_supported() {
    let out = synopsize("shell", "[]", 1024).unwrap();
    assert!(out.contains("empty array"));
}
#[test]
fn top_level_scalar_json_is_supported() {
    let out = synopsize("shell", "\"hello world\"", 1024).unwrap();
    assert!(has_label(&out, "kind"));
    assert!(has_label(&out, "root"));
}
#[test]
fn deterministic_across_runs() {
    let value = json!({"a": "one", "b": "two", "c": "three", "nested": {"x": 1, "y": 2}});
    let raw = serde_json::to_string(&value).unwrap();
    assert_eq!(
        synopsize("shell", &raw, 1200),
        synopsize("shell", &raw, 1200)
    );
}
#[test]
fn tool_name_does_not_influence_json_synopsis() {
    assert_eq!(
        synopsize("shell", "{\"a\":1}", 1024),
        synopsize("task_list", "{\"a\":1}", 1024)
    );
}
#[test]
fn streaming_scan_rejects_unterminated_string() {
    assert_eq!(count_tokens_streaming("{\"a\": \"oops"), None);
}
#[test]
fn streaming_scan_rejects_unbalanced_brackets() {
    assert_eq!(count_tokens_streaming("[1,2,3"), None);
    assert_eq!(count_tokens_streaming("1,2,3]"), None);
}
#[test]
fn streaming_scan_counts_simple_object() {
    assert_eq!(count_tokens_streaming("{\"a\":1,\"b\":2}"), Some(5));
}
#[test]
fn streaming_scan_handles_escaped_quotes_in_strings() {
    let raw = "{\"msg\":\"he said \\\"hi\\\"\"}";
    assert!(count_tokens_streaming(raw).is_some());
    assert!(has_label(&synopsize("shell", raw, 1024).unwrap(), "kind"));
}

// -- Code tests --
#[test]
fn rust_code_produces_code_synopsis() {
    let code = "use std::collections::HashMap;\nuse std::io::Read;\n\nstruct Config {\n    name: String,\n    port: u16,\n}\n\nimpl Config {\n    fn new(name: &str) -> Self {\n        Config { name: name.to_string(), port: 8080 }\n    }\n}\n\nfn main() {\n    let config = Config::new(\"server\");\n}\n";
    let out = synopsize("shell", code, 1200).expect("code synopsis present");
    assert!(out.contains("code"), "expected kind=code: {out}");
    assert!(has_label(&out, "lines"), "lines missing: {out}");
    assert!(has_label(&out, "imports"), "imports missing: {out}");
    assert!(has_label(&out, "symbols"), "symbols missing: {out}");
    assert!(
        has_label(&out, "suggested grep terms"),
        "grep terms missing: {out}"
    );
}
#[test]
fn code_synopsis_contains_import_statements() {
    let code = "import os\nimport sys\nfrom pathlib import Path\n\ndef main():\n    pass\n";
    let out = synopsize("shell", code, 1200).unwrap();
    assert!(out.contains("import os"));
    assert!(out.contains("import sys"));
    assert!(out.contains("from pathlib import Path"));
}
#[test]
fn code_synopsis_contains_symbols() {
    let code = "fn process_data(input: &str) -> String {\n    input.to_string()\n}\n\nstruct Config {\n    port: u16,\n}\n";
    let out = synopsize("shell", code, 1200).unwrap();
    assert!(out.contains("process_data"));
    assert!(out.contains("Config"));
}
#[test]
fn code_synopsis_respects_budget() {
    let code = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\nstruct S1 {}\nstruct S2 {}\nimport os\nimport sys\n";
    let out = synopsize("shell", code, 80).unwrap();
    assert!(out.len() <= 80);
}
#[test]
fn code_synopsis_is_deterministic() {
    let code = "fn hello() {}\nstruct Greeting;\nimport std::io;\n";
    assert_eq!(
        synopsize("shell", code, 1200),
        synopsize("shell", code, 1200)
    );
}
#[test]
fn python_code_is_classified_as_code() {
    let code = "import os\nimport sys\nfrom typing import List\n\ndef process(items: List[str]) -> None:\n    pass\n\nclass Handler:\n    def handle(self, data):\n        return data\n";
    let out = synopsize("shell", code, 1200).unwrap();
    assert!(out.contains("code"));
    assert!(out.contains("process"));
    assert!(out.contains("Handler"));
}
#[test]
fn c_code_is_classified_as_code() {
    let code = "#include <stdio.h>\n#include <stdlib.h>\n\nint main(int argc, char *argv[]) {\n    printf(\"hello\\n\");\n    return 0;\n}\n";
    let out = synopsize("shell", code, 1200).unwrap();
    assert!(out.contains("code"));
    assert!(has_label(&out, "imports"));
}
#[test]
fn go_code_is_classified_as_code() {
    let code = "package main\n\nimport \"fmt\"\n\nfunc main() {\n    fmt.Println(\"hello\")\n}\n";
    let out = synopsize("shell", code, 1200).unwrap();
    assert!(out.contains("code"));
}
#[test]
fn short_prose_is_not_classified_as_code() {
    let out = synopsize(
        "shell",
        "This is a simple message about the function of the system.",
        1024,
    )
    .unwrap();
    assert!(out.contains("text"), "prose should be text: {out}");
}
#[test]
fn log_output_is_not_classified_as_code() {
    let log = "2024-01-15 10:30:00 INFO Starting server\n2024-01-15 10:30:01 ERROR Connection failed\n2024-01-15 10:30:02 WARN Retry\n2024-01-15 10:30:03 INFO Connected\n";
    let out = synopsize("shell", log, 1024).unwrap();
    assert!(out.contains("text"), "log should be text: {out}");
}

// -- Text/log tests --
#[test]
fn plain_text_produces_text_synopsis() {
    let out = synopsize(
        "shell",
        "Hello world.\nThis is a simple text output.\nNo special markers here.\n",
        1024,
    )
    .unwrap();
    assert!(out.contains("text"));
    assert!(has_label(&out, "lines"));
}
#[test]
fn text_synopsis_contains_line_count() {
    let out = synopsize("shell", "line 1\nline 2\nline 3\nline 4\nline 5\n", 1024).unwrap();
    assert!(out.contains("lines"));
    assert!(out.contains("5"));
}
#[test]
fn text_synopsis_detects_notable_markers() {
    let log = "INFO: Starting\nERROR: connection refused\nTraceback (most recent):\nFAILED: test_1\npanic: runtime error\n";
    let out = synopsize("shell", log, 1024).unwrap();
    assert!(has_label(&out, "notable markers"));
    assert!(out.contains("error:"));
    assert!(out.contains("FAILED"));
    assert!(out.contains("panic"));
    assert!(out.contains("Traceback"));
}
#[test]
fn text_synopsis_detects_markdown_sections() {
    let md =
        "# Introduction\nSome text.\n## Setup\nSteps.\n## Results\nGood.\n### Details\nMore.\n";
    let out = synopsize("shell", md, 1024).unwrap();
    assert!(has_label(&out, "sections"));
    assert!(out.contains("Introduction"));
    assert!(out.contains("Setup"));
    assert!(out.contains("Results"));
}
#[test]
fn text_synopsis_suggested_grep_terms_from_headers() {
    let md = "# Authentication\nDetails.\n## OAuth2\nFlow.\n## JWT\nTokens.\n";
    let out = synopsize("shell", md, 1024).unwrap();
    assert!(has_label(&out, "suggested grep terms"));
}
#[test]
fn text_synopsis_respects_budget() {
    let text = "ERROR: failed\nFAILED: test\npanic: crash\n# One\n## Two\n### Three\n";
    let out = synopsize("shell", text, 100).unwrap();
    assert!(out.len() <= 100);
}
#[test]
fn text_synopsis_is_deterministic() {
    let text = "ERROR: failure\n# Header A\n## Header B\nSome content.\n";
    assert_eq!(
        synopsize("shell", text, 1200),
        synopsize("shell", text, 1200)
    );
}
#[test]
fn error_count_reflected_in_markers() {
    let log = "error: first\nerror: second\nerror: third\nINFO: ok\n";
    let out = synopsize("shell", log, 1024).unwrap();
    assert!(out.contains("error:(3)"), "expected error:(3): {out}");
}
#[test]
fn no_notable_markers_omits_section() {
    let out = synopsize("shell", "Just a regular text.\nNothing special.\n", 1024).unwrap();
    assert!(!has_label(&out, "notable markers"));
}
#[test]
fn no_sections_in_plain_text() {
    let out = synopsize("shell", "Just a regular text.\nNothing special.\n", 1024).unwrap();
    assert!(!has_label(&out, "sections"));
}

// -- Binary no-op tests --
#[test]
fn binary_with_null_bytes_returns_none() {
    let mut bytes = vec![b'h', b'e', b'l', b'l', b'o'];
    bytes.push(0);
    bytes.extend_from_slice(b"world");
    assert_eq!(
        synopsize("shell", &String::from_utf8_lossy(&bytes), 1024),
        None
    );
}
#[test]
fn binary_with_high_control_char_ratio_returns_none() {
    let mut s = String::new();
    for _ in 0..90 {
        s.push('x');
    }
    for _ in 0..10 {
        s.push('\x01');
    }
    assert_eq!(synopsize("shell", &s, 1024), None);
}
#[test]
fn text_with_newlines_is_not_binary() {
    assert!(synopsize("shell", &"line\n".repeat(100), 1024).is_some());
}
#[test]
fn binary_input_emits_no_synopsis_header() {
    let mut s = String::from("hello");
    s.push('\0');
    s.push_str("world");
    assert_eq!(synopsize("shell", &s, 1024), None);
}

// -- CSV/TSV/XML/YAML routing tests --
#[test]
fn csv_input_gets_text_synopsis() {
    let out = synopsize("shell", "name,age,city\nAlice,30,NYC\nBob,25,LA\n", 1024).unwrap();
    assert!(out.contains("text"));
    assert!(has_label(&out, "lines"));
}
#[test]
fn tsv_input_gets_text_synopsis() {
    let out = synopsize(
        "shell",
        "name\tage\tcity\nAlice\t30\tNYC\nBob\t25\tLA\n",
        1024,
    )
    .unwrap();
    assert!(out.contains("text"));
}
#[test]
fn xml_input_gets_text_synopsis() {
    let xml = "<?xml version=\"1.0\"?>\n<root>\n  <item id=\"1\">\n    <name>Alice</name>\n  </item>\n</root>\n";
    let out = synopsize("shell", xml, 1024).unwrap();
    assert!(out.contains("text"));
}
#[test]
fn yaml_like_input_gets_text_synopsis() {
    let out = synopsize(
        "shell",
        "name: Alice\nage: 30\ncity: NYC\nitems:\n  - one\n  - two\n",
        1024,
    )
    .unwrap();
    assert!(out.contains("text"));
}
#[test]
fn csv_does_not_panic_with_malformed_input() {
    assert!(synopsize("shell", "a,b,c\n1,2\n3,4,5,6,7\n", 1024).is_some());
}
#[test]
fn xml_with_code_like_content_still_text() {
    let out = synopsize(
        "shell",
        "<div class=\"container\">\n  <span>Alice</span>\n</div>\n",
        1024,
    )
    .unwrap();
    assert!(out.contains("text"));
}

// -- Budget and pathological tests --
#[test]
fn tiny_budget_returns_none_for_all_kinds() {
    assert_eq!(synopsize("shell", "{\"a\":1}", 1), None);
    assert_eq!(synopsize("shell", "fn main() {}\n", 1), None);
    assert_eq!(synopsize("shell", "hello world\n", 1), None);
}
#[test]
fn code_synopsis_never_exceeds_budget() {
    let code = "fn a() {}\nfn b() {}\nfn c() {}\nstruct S1 {}\nstruct S2 {}\nuse std::io;\nimport os\nimport sys\n";
    for budget in [20, 50, 100, 200, 500, 1000] {
        if let Some(out) = synopsize("shell", code, budget) {
            assert!(out.len() <= budget);
        }
    }
}
#[test]
fn text_synopsis_never_exceeds_budget() {
    let text = "ERROR: failure\nFAILED: test\npanic: crash\n# One\n## Two\n### Three\nMore text.\n";
    for budget in [20, 50, 100, 200, 500, 1000] {
        if let Some(out) = synopsize("shell", text, budget) {
            assert!(out.len() <= budget);
        }
    }
}
#[test]
fn very_long_code_input_does_not_panic() {
    let mut code = String::new();
    for i in 0..10_000 {
        code.push_str(&format!("fn func_{i}() {{\n    println!(\"{i}\");\n}}\n"));
    }
    if let Some(out) = synopsize("shell", &code, 4096) {
        assert!(out.len() <= 4096);
    }
}
#[test]
fn very_long_text_input_does_not_panic() {
    let mut text = String::new();
    for i in 0..10_000 {
        text.push_str(&format!("Line {i}: some content.\n"));
    }
    if let Some(out) = synopsize("shell", &text, 4096) {
        assert!(out.len() <= 4096);
    }
}

// -- Deterministic output ordering --
#[test]
fn code_synopsis_output_ordering_is_stable() {
    let code = "import os\nimport sys\nfn process() {}\nstruct Config {}\nclass Handler {}\nenum State {}\ntrait Printable {}\n";
    let out = synopsize("shell", code, 2000).unwrap();
    let (kp, lp, ip, sp, gp) = (
        out.find("- kind:").unwrap(),
        out.find("- lines:").unwrap(),
        out.find("- imports:").unwrap(),
        out.find("- symbols:").unwrap(),
        out.find("- suggested grep terms:").unwrap(),
    );
    assert!(kp < lp && lp < ip && ip < sp && sp < gp);
}
#[test]
fn text_synopsis_output_ordering_is_stable() {
    let text = "# Title\nERROR: failure\nFAILED: test\n## Section\nSome text.\n";
    let out = synopsize("shell", text, 2000).unwrap();
    let (kp, lp, sp, mp, gp) = (
        out.find("- kind:").unwrap(),
        out.find("- lines:").unwrap(),
        out.find("- sections:").unwrap(),
        out.find("- notable markers:").unwrap(),
        out.find("- suggested grep terms:").unwrap(),
    );
    assert!(kp < lp && lp < sp && sp < mp && mp < gp);
}
#[test]
fn json_synopsis_output_ordering_is_stable() {
    let value = json!({"users": [{"name": "a"}], "config": {"host": "x"}, "status": "ok"});
    let raw = serde_json::to_string(&value).unwrap();
    let out = synopsize("shell", &raw, 2000).unwrap();
    assert!(out.find("- kind:").unwrap() < out.find("- root:").unwrap());
}

// -- Edge cases --
#[test]
fn single_line_text_gets_synopsis() {
    let out = synopsize("shell", "just one line", 1024).unwrap();
    assert!(out.contains("text"));
    assert!(out.contains("1"));
}
#[test]
fn whitespace_only_returns_none() {
    assert_eq!(synopsize("shell", "   \n\t\n   ", 1024), None);
}
#[test]
fn json_with_leading_whitespace_still_parses() {
    let out = synopsize("shell", "  \n  {\"key\": \"value\"}", 1024).unwrap();
    assert!(has_label(&out, "root"));
}
#[test]
fn shebang_line_not_treated_as_markdown() {
    let out = synopsize("shell", "#!/bin/bash\necho hello\nls -la\n", 1024).unwrap();
    assert!(out.len() <= 1024);
}
#[test]
fn non_json_text_with_code_mentions_is_text() {
    let text = "The function of this system is to process data.\nWe use a struct to hold configuration.\nThe class of problems is NP-hard.\n";
    let out = synopsize("shell", text, 1024).unwrap();
    assert!(
        out.contains("text"),
        "prose mentioning code terms should be text: {out}"
    );
}
