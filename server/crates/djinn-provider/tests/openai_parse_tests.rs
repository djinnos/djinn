use std::collections::BTreeMap;

use djinn_provider::message::ContentBlock;
use djinn_provider::provider::StreamEvent;
use djinn_provider::provider::format::openai::parse_openai_line;

#[test]
fn test_parse_text_delta() {
    let line = r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null,"index":0}]}"#;
    let mut acc = BTreeMap::new();
    let events = parse_openai_line(line, &mut acc);
    assert_eq!(events.len(), 1);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "hello"),
        _ => panic!("expected text delta"),
    }
}

#[test]
fn test_parse_empty_content_skipped() {
    let line = r#"{"choices":[{"delta":{"content":""},"finish_reason":null,"index":0}]}"#;
    let mut acc = BTreeMap::new();
    let events = parse_openai_line(line, &mut acc);
    assert!(events.is_empty());
}

#[test]
fn test_parse_tool_call_accumulated() {
    // First chunk: tool call start
    let line1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"shell","arguments":""}}]},"finish_reason":null}]}"#;
    // Second chunk: arguments fragment
    let line2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
    // Final chunk: finish_reason=tool_calls
    let line3 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

    let mut acc = BTreeMap::new();
    let e1 = parse_openai_line(line1, &mut acc);
    assert!(e1.is_empty(), "no events on first tool chunk");
    assert!(!acc.is_empty());

    let e2 = parse_openai_line(line2, &mut acc);
    assert!(e2.is_empty(), "no events while accumulating");

    let e3 = parse_openai_line(line3, &mut acc);
    assert_eq!(e3.len(), 1);
    match &e3[0] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
            assert_eq!(id.as_str(), "call_abc");
            assert_eq!(name.as_str(), "shell");
            assert_eq!(input["cmd"].as_str(), Some("ls"));
        }
        _ => panic!("expected tool use"),
    }
    assert!(acc.is_empty(), "accumulator cleared after emit");
}

#[test]
fn test_parse_invalid_json_ignored() {
    let mut acc = BTreeMap::new();
    let events = parse_openai_line("{not-json", &mut acc);
    assert!(events.is_empty());
    assert!(acc.is_empty());
}

#[test]
fn test_parse_missing_choices_with_usage_only() {
    let line = r#"{"usage":{"prompt_tokens":7}}"#;
    let mut acc = BTreeMap::new();
    let events = parse_openai_line(line, &mut acc);
    assert_eq!(events.len(), 1);
    match &events[0] {
        StreamEvent::Usage(u) => {
            assert_eq!(u.input, 7);
            assert_eq!(u.output, 0);
        }
        _ => panic!("expected usage"),
    }
}

#[test]
fn test_parse_reasoning_content() {
    let line = r#"{"choices":[{"delta":{"reasoning_content":"let me think..."},"finish_reason":null,"index":0}]}"#;
    let mut acc = BTreeMap::new();
    let events = parse_openai_line(line, &mut acc);
    assert_eq!(events.len(), 1);
    match &events[0] {
        StreamEvent::Thinking(text) => assert_eq!(text, "let me think..."),
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn test_parse_reasoning_details() {
    let line = r#"{"choices":[{"delta":{"reasoning_details":"step 1: analyze"},"finish_reason":null,"index":0}]}"#;
    let mut acc = BTreeMap::new();
    let events = parse_openai_line(line, &mut acc);
    assert_eq!(events.len(), 1);
    match &events[0] {
        StreamEvent::Thinking(text) => assert_eq!(text, "step 1: analyze"),
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn test_parse_reasoning_content_with_text() {
    // Some models send both reasoning and content in same chunk
    let line = r#"{"choices":[{"delta":{"reasoning_content":"thinking","content":"hello"},"finish_reason":null,"index":0}]}"#;
    let mut acc = BTreeMap::new();
    let events = parse_openai_line(line, &mut acc);
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], StreamEvent::Thinking(t) if t == "thinking"));
    assert!(
        matches!(&events[1], StreamEvent::Delta(ContentBlock::Text { text }) if text == "hello")
    );
}

#[test]
fn test_parse_empty_reasoning_content_skipped() {
    let line = r#"{"choices":[{"delta":{"reasoning_content":""},"finish_reason":null,"index":0}]}"#;
    let mut acc = BTreeMap::new();
    let events = parse_openai_line(line, &mut acc);
    assert!(events.is_empty());
}

#[test]
fn test_parse_done_sentinel_ignored() {
    let mut acc = BTreeMap::new();
    let events = parse_openai_line("[DONE]", &mut acc);
    assert!(events.is_empty());
    assert!(acc.is_empty());
}

#[test]
fn test_parse_parallel_tool_calls() {
    let mut acc = BTreeMap::new();

    // Chunk 1: two tool calls start
    let line1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":""}},{"index":1,"id":"call_2","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
    let e1 = parse_openai_line(line1, &mut acc);
    assert!(e1.is_empty(), "no events on first tool chunk");
    assert_eq!(acc.len(), 2);

    // Chunk 2: arguments for index 0
    let line2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
    let e2 = parse_openai_line(line2, &mut acc);
    assert!(e2.is_empty());

    // Chunk 3: arguments for index 1
    let line3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"/tmp\"}"}}]},"finish_reason":null}]}"#;
    let e3 = parse_openai_line(line3, &mut acc);
    assert!(e3.is_empty());

    // Chunk 4: finish
    let line4 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
    let e4 = parse_openai_line(line4, &mut acc);
    assert_eq!(e4.len(), 2);

    match &e4[0] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "shell");
            assert_eq!(input["cmd"].as_str(), Some("ls"));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
    match &e4[1] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
            assert_eq!(id, "call_2");
            assert_eq!(name, "read");
            assert_eq!(input["path"].as_str(), Some("/tmp"));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
    assert!(acc.is_empty(), "accumulator cleared after emit");
}

#[test]
fn test_parse_parallel_tool_calls_with_thinking() {
    let mut acc = BTreeMap::new();

    // Chunk 1: reasoning + two tool calls start
    let line1 = r#"{"choices":[{"delta":{"reasoning_content":"Let me run these commands","tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":""}},{"index":1,"id":"call_2","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
    let e1 = parse_openai_line(line1, &mut acc);
    assert_eq!(e1.len(), 1);
    match &e1[0] {
        StreamEvent::Thinking(text) => assert_eq!(text, "Let me run these commands"),
        other => panic!("expected Thinking, got {other:?}"),
    }

    // Chunk 2: arguments for index 0
    let line2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
    parse_openai_line(line2, &mut acc);

    // Chunk 3: arguments for index 1
    let line3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"/tmp\"}"}}]},"finish_reason":null}]}"#;
    parse_openai_line(line3, &mut acc);

    // Chunk 4: finish
    let line4 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
    let e4 = parse_openai_line(line4, &mut acc);
    assert_eq!(e4.len(), 2);

    match &e4[0] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, .. }) => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "shell");
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
    match &e4[1] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, .. }) => {
            assert_eq!(id, "call_2");
            assert_eq!(name, "read");
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn test_parse_thinking_before_tool_call() {
    let mut acc = BTreeMap::new();

    // Chunk 1: reasoning token
    let line1 =
        r#"{"choices":[{"delta":{"reasoning_content":"thinking step 1"},"finish_reason":null}]}"#;
    let e1 = parse_openai_line(line1, &mut acc);
    assert_eq!(e1.len(), 1);
    assert!(matches!(&e1[0], StreamEvent::Thinking(t) if t == "thinking step 1"));

    // Chunk 2: more reasoning
    let line2 =
        r#"{"choices":[{"delta":{"reasoning_content":"thinking step 2"},"finish_reason":null}]}"#;
    let e2 = parse_openai_line(line2, &mut acc);
    assert_eq!(e2.len(), 1);
    assert!(matches!(&e2[0], StreamEvent::Thinking(t) if t == "thinking step 2"));

    // Chunk 3: tool call
    let line3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#;
    let e3 = parse_openai_line(line3, &mut acc);
    assert!(e3.is_empty());

    // Chunk 4: finish
    let line4 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
    let e4 = parse_openai_line(line4, &mut acc);
    assert_eq!(e4.len(), 1);
    match &e4[0] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "shell");
            assert_eq!(input["cmd"].as_str(), Some("ls"));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}
