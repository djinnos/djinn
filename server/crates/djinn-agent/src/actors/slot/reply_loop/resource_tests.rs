// Copyright (c) djinnos, Inc. and affiliates. All rights reserved.
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Regression tests for MCP resource content rendering.
//!
//! Exercises the `format_resource_contents` helper directly so the tests
//! do not need a live MCP peer or full `AgentToolDispatcher` construction.
//! Schema gating and registry argument validation are covered by the
//! adjacent `resource_tests.rs` in `mcp_client` (task jdgb).

use super::format_resource_contents;
use crate::mcp_client::MAX_MCP_RESOURCE_TEXT_BYTES;
use rmcp::model::ResourceContents;

#[test]
fn read_text_resource_renders_uri_mime_and_text() {
    let contents = vec![ResourceContents::text("hello world", "file:///test.txt")];
    let result = format_resource_contents(&contents);
    assert!(
        result.contains("Resource: file:///test.txt"),
        "expected URI, got: {result}"
    );
    // ResourceContents::text() sets mime_type to Some("text").
    assert!(
        result.contains("MIME: text"),
        "expected MIME line, got: {result}"
    );
    assert!(
        result.contains("hello world"),
        "expected text content, got: {result}"
    );
}

#[test]
fn read_binary_resource_omits_content() {
    let contents = vec![ResourceContents::blob("base64data", "file:///test.bin")];
    let result = format_resource_contents(&contents);
    assert!(
        result.contains("Resource: file:///test.bin"),
        "expected URI, got: {result}"
    );
    // blob() sets mime_type to None; dispatch defaults to application/octet-stream.
    assert!(
        result.contains("MIME: application/octet-stream"),
        "expected default MIME, got: {result}"
    );
    assert!(
        result.contains("binary resource omitted: 10 bytes"),
        "expected omission message, got: {result}"
    );
    assert!(
        !result.contains("base64data"),
        "binary content must not leak into result"
    );
}

#[test]
fn read_oversized_text_resource_omits_content() {
    let huge = "x".repeat(MAX_MCP_RESOURCE_TEXT_BYTES + 1);
    let contents = vec![ResourceContents::text(&huge, "file:///huge.txt")];
    let result = format_resource_contents(&contents);
    assert!(
        result.contains("Resource: file:///huge.txt"),
        "expected URI, got: {result}"
    );
    assert!(
        result.contains("MIME: text"),
        "expected MIME line, got: {result}"
    );
    assert!(
        result.contains("resource omitted:"),
        "expected omission message, got: {result}"
    );
    assert!(
        result.contains("exceeds 10 MiB limit"),
        "expected size limit message, got: {result}"
    );
    // Verify the huge content is not injected.
    let prefix: String = "x".repeat(20);
    assert!(
        !result.contains(&prefix),
        "oversized content must not leak into result"
    );
}
