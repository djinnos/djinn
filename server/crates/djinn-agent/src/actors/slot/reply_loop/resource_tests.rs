// Copyright (c) djinnos, Inc. and affiliates. All rights reserved.
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use crate::mcp_client::McpToolRegistry;
use crate::mcp_client::MAX_MCP_RESOURCE_TEXT_BYTES;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

#[cfg(test)]
use rmcp::model::{
    ListResourcesResult, ReadResourceResult, Resource as RmcpResource, ResourceContents,
};

/// Test-only helper to inject resource discovery results into the `McpToolRegistry`.
///
/// This avoids needing a real rmcp peer in the test by filling the routing state with
/// a fake `resource_servers` set and a test dispatch hook that responds to
/// `list_mcp_resources` / `read_mcp_resource`.
struct ResourceTestRegistry {
    registry: McpToolRegistry,
}

impl ResourceTestRegistry {
    fn new(resource_servers: Vec<String>) -> Self {
        let mut sorted = resource_servers;
        sorted.sort();
        let mut resource_servers_set = HashSet::new();
        for name in &sorted {
            resource_servers_set.insert(name.clone());
        }
        let routing = Arc::new(RwLock::new(crate::mcp_client::RoutingState {
            tool_to_server: HashMap::new(),
            namespaced_to_original: HashMap::new(),
            peers: HashMap::new(),
            request_timeouts: HashMap::new(),
            unavailable: HashSet::new(),
            server_instructions: BTreeMap::new(),
            tool_fingerprints: HashMap::new(),
            resource_servers: resource_servers_set,
        }));
        Self {
            registry: McpToolRegistry {
                routing,
                tool_schemas: Vec::new(),
                server_instructions: BTreeMap::new(),
                resource_servers: sorted,
                test_dispatch: None,
            },
        }
    }

    fn with_test_dispatch<F>(self, f: F) -> McpToolRegistry
    where
        F: Fn(
                &str,
                Option<serde_json::Map<String, serde_json::Value>>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>,
            > + Send
            + Sync
            + 'static,
    {
        McpToolRegistry {
            routing: self.registry.routing,
            tool_schemas: self.registry.tool_schemas,
            server_instructions: self.registry.server_instructions,
            resource_servers: self.registry.resource_servers,
            test_dispatch: Some(Arc::new(f)),
        }
    }
}

#[cfg(test)]
mod resource_rendering_tests {
    use super::*;
    use crate::actors::slot::reply_loop::AgentToolDispatcher;

    fn make_dispatcher(registry: McpToolRegistry) -> AgentToolDispatcher {
        let app_state = crate::context::AgentContext::default_test();
        AgentToolDispatcher::new(
            &app_state,
            &crate::test_helpers::stub_services(),
            Some(&registry),
            None,
        )
    }

    #[test]
    fn read_text_resource_renders_uri_mime_and_text() {
        let registry = ResourceTestRegistry::new(vec!["test-server".to_string()])
            .with_test_dispatch(|tool, args| {
                Box::pin(async move {
                    if tool != "read_mcp_resource" {
                        return Err("unexpected tool".to_string());
                    }
                    let uri = args
                        .as_ref()
                        .and_then(|a| a.get("uri"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("file:///default.txt")
                        .to_string();
                    let result = ReadResourceResult::new(vec![ResourceContents::text(
                        "hello world",
                        &uri,
                    )]);
                    serde_json::to_value(result).map_err(|e| e.to_string())
                })
            });
        let dispatcher = make_dispatcher(registry);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(dispatcher.dispatch_resource_tool(
                "read_mcp_resource",
                Some(serde_json::json!({"server": "test-server", "uri": "file:///test.txt"}).as_object().unwrap().clone()),
            ))
            .unwrap();
        assert!(result.contains("Resource: file:///test.txt"));
        assert!(result.contains("MIME: text/plain"));
        assert!(result.contains("hello world"));
    }

    #[test]
    fn read_binary_resource_omits_content() {
        let registry = ResourceTestRegistry::new(vec!["test-server".to_string()])
            .with_test_dispatch(|tool, args| {
                Box::pin(async move {
                    if tool != "read_mcp_resource" {
                        return Err("unexpected tool".to_string());
                    }
                    let uri = args
                        .as_ref()
                        .and_then(|a| a.get("uri"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("file:///default.bin")
                        .to_string();
                    let result = ReadResourceResult::new(vec![ResourceContents::blob(
                        "base64data",
                        &uri,
                    )]);
                    serde_json::to_value(result).map_err(|e| e.to_string())
                })
            });
        let dispatcher = make_dispatcher(registry);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(dispatcher.dispatch_resource_tool(
                "read_mcp_resource",
                Some(serde_json::json!({"server": "test-server", "uri": "file:///test.bin"}).as_object().unwrap().clone()),
            ))
            .unwrap();
        assert!(result.contains("Resource: file:///test.bin"));
        assert!(result.contains("MIME: application/octet-stream"));
        assert!(result.contains("binary resource omitted: 10 bytes"));
        assert!(!result.contains("base64data"));
    }

    #[test]
    fn read_oversized_text_resource_omits_content() {
        let huge = "x".repeat(MAX_MCP_RESOURCE_TEXT_BYTES + 1);
        let registry = ResourceTestRegistry::new(vec!["test-server".to_string()])
            .with_test_dispatch(move |tool, args| {
                let huge = huge.clone();
                Box::pin(async move {
                    if tool != "read_mcp_resource" {
                        return Err("unexpected tool".to_string());
                    }
                    let uri = args
                        .as_ref()
                        .and_then(|a| a.get("uri"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("file:///default.txt")
                        .to_string();
                    let result = ReadResourceResult::new(vec![ResourceContents::text(
                        &huge,
                        &uri,
                    )]);
                    serde_json::to_value(result).map_err(|e| e.to_string())
                })
            });
        let dispatcher = make_dispatcher(registry);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(dispatcher.dispatch_resource_tool(
                "read_mcp_resource",
                Some(serde_json::json!({"server": "test-server", "uri": "file:///huge.txt"}).as_object().unwrap().clone()),
            ))
            .unwrap();
        assert!(result.contains("Resource: file:///huge.txt"));
        assert!(result.contains("MIME: text/plain"));
        assert!(result.contains("resource omitted:"));
        assert!(result.contains("exceeds 10 MiB limit"));
        assert!(!result.contains("xxxxxxxxxxxxxxxxxxxx"));
    }
}
