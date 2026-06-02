use async_stream::stream;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::pin::Pin;

use crate::message::{CacheBreakpoint, ContentBlock, Conversation};
use crate::provider::client::ApiClient;
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent, TokenUsage, ToolChoice};

const ANTHROPIC_CACHE_BREAKPOINT_KEY: &str = "anthropic_cache_breakpoint";
#[cfg(test)]
const ANTHROPIC_STABLE_PREFIX_KIND: &str = "stable_prefix";

/// Anthropic's API accepts at most this many `cache_control` breakpoint markers
/// per request; exceeding it returns a 400. djinn distributes markers across
/// tool definitions, system blocks, and the trailing message breakpoint, none of
/// which individually knows the global count — so we enforce a hard cap over the
/// fully-assembled request body just before it ships.
const MAX_CACHE_CONTROL_MARKERS: usize = 4;

#[derive(Debug, Clone, PartialEq)]
struct AnthropicSystemBlock {
    text: String,
    cache_control: Option<Value>,
}

pub struct AnthropicProvider {
    config: ProviderConfig,
    client: ApiClient,
    /// Hash of the `cache_control`-marked stable prefix from this provider
    /// instance's previous request, used by the B3 drift guard to detect a
    /// supposedly-stable prefix mutating across consecutive turns. `Mutex` because
    /// [`Self::stream`] takes `&self`; uncontended (one request in flight per
    /// instance) so the lock is effectively free.
    last_prefix_hash: std::sync::Mutex<Option<u64>>,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: ApiClient::new(),
            last_prefix_hash: std::sync::Mutex::new(None),
        }
    }

    fn maybe_cache_control(message: &djinn_core::message::Message) -> Option<Value> {
        message
            .metadata
            .as_ref()
            .and_then(|meta| meta.provider_data.as_ref())
            .and_then(|data| data.get(ANTHROPIC_CACHE_BREAKPOINT_KEY))
            .and_then(|value| serde_json::from_value::<CacheBreakpoint>(value.clone()).ok())
            .map(|breakpoint| {
                let mut obj = serde_json::Map::new();
                obj.insert("type".to_string(), json!("ephemeral"));
                if let Some(kind) = breakpoint.kind {
                    obj.insert("kind".to_string(), json!(kind));
                }
                Value::Object(obj)
            })
    }

    /// Convert system messages into Anthropic system blocks with cache_control.
    ///
    /// # Anthropic prompt-cache semantics (ADR-043 §8)
    ///
    /// The full stable ordering spans both chat-layer system blocks and
    /// provider-owned request blocks:
    ///
    ///   1. base system prompt                    (`chat.rs` system block)
    ///   2. tool definitions                      (provider request assembly)
    ///   3. project/repository context            (`chat.rs` system blocks)
    ///   4. dynamic task/request context tail     (`chat.rs` trailing uncached block)
    ///
    /// This formatter only serializes the `system` blocks coming from
    /// `server/src/server/chat.rs`, so its responsibility is narrower: preserve
    /// the stable system-message taxonomy emitted there and consume the explicit
    /// `anthropic_cache_breakpoint` / `stable_prefix` metadata contract. When that
    /// metadata is present, every serialized system block except the last is part
    /// of the cacheable prefix and receives `cache_control: {"type":"ephemeral"}`.
    ///
    /// The final system block must remain uncached because it represents the
    /// dynamic tail. Non-Anthropic providers ignore this metadata and continue to
    /// serialize the same content as plain text.
    fn system_blocks(conversation: &Conversation) -> Vec<AnthropicSystemBlock> {
        conversation
            .messages
            .iter()
            .filter(|message| message.role == djinn_core::message::Role::System)
            .flat_map(|message| {
                let cache_control = Self::maybe_cache_control(message);
                // Collect non-empty text blocks first, then apply cache_control
                // to all but the last block (the stable-prefix boundary).
                let non_empty_blocks: Vec<&str> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                let block_count = non_empty_blocks.len();
                non_empty_blocks
                    .into_iter()
                    .enumerate()
                    .map(move |(index, text)| {
                        let cc = if cache_control.is_some() && index + 1 < block_count {
                            cache_control.clone()
                        } else {
                            None
                        };
                        AnthropicSystemBlock {
                            text: text.to_string(),
                            cache_control: cc,
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn serialize_system_blocks(blocks: &[AnthropicSystemBlock]) -> Option<Value> {
        if blocks.is_empty() {
            return None;
        }
        if blocks.len() == 1 && blocks[0].cache_control.is_none() {
            return Some(Value::String(blocks[0].text.clone()));
        }

        Some(Value::Array(
            blocks
                .iter()
                .map(|block| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("type".to_string(), json!("text"));
                    obj.insert("text".to_string(), json!(block.text));
                    if let Some(cache_control) = &block.cache_control {
                        obj.insert("cache_control".to_string(), cache_control.clone());
                    }
                    Value::Object(obj)
                })
                .collect(),
        ))
    }

    fn tool_definition_cache_control(conversation: &Conversation) -> Option<Value> {
        conversation
            .messages
            .iter()
            .find(|message| message.role == djinn_core::message::Role::System)
            .and_then(Self::maybe_cache_control)
    }

    fn serialize_tools_for_request(conversation: &Conversation, tools: &[Value]) -> Option<Value> {
        if tools.is_empty() {
            return None;
        }

        let cache_control = Self::tool_definition_cache_control(conversation);
        Some(Value::Array(
            tools
                .iter()
                .enumerate()
                .map(|(index, tool)| {
                    let mut tool_obj = tool.clone();
                    if index == 0
                        && let Some(cache_control) = &cache_control
                        && let Some(obj) = tool_obj.as_object_mut()
                    {
                        obj.insert("cache_control".to_string(), cache_control.clone());
                    }
                    tool_obj
                })
                .collect(),
        ))
    }

    /// Whether prompt caching is active for this conversation (i.e. at least one
    /// system message carries the `anthropic_cache_breakpoint` metadata).
    fn has_cache_metadata(conversation: &Conversation) -> bool {
        conversation.messages.iter().any(|m| {
            m.role == djinn_core::message::Role::System && Self::maybe_cache_control(m).is_some()
        })
    }

    /// Inject a `cache_control: {"type": "ephemeral"}` marker on the last
    /// content block of the last message in the serialized messages array.
    ///
    /// This creates a message-level cache breakpoint so that the entire
    /// conversation prefix (system + tools + all messages up to the latest turn)
    /// becomes cacheable across consecutive requests within the same session.
    fn add_message_cache_breakpoint(messages: &mut [Value]) {
        if let Some(last_msg) = messages.last_mut()
            && let Some(content) = last_msg.get_mut("content").and_then(Value::as_array_mut)
            && let Some(last_block) = content.last_mut()
            && let Some(obj) = last_block.as_object_mut()
        {
            obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        }
    }

    /// Visit every `cache_control` marker in the assembled request body in
    /// descending cache-value priority order, invoking `visit` once per marker.
    ///
    /// Priority order (most cache-valuable first):
    ///   1. tool definitions   (largest, most stable prefix — highest reuse)
    ///   2. system blocks       (base prompt / project context, earliest first)
    ///   3. trailing message breakpoint (conversation-prefix boundary)
    ///
    /// `visit` receives a mutable reference to the JSON object that carries a
    /// `cache_control` key; it may remove that key to drop the marker.
    fn for_each_cache_marker(
        body: &mut Value,
        mut visit: impl FnMut(&mut serde_json::Map<String, Value>),
    ) {
        // 1. tools
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools.iter_mut() {
                if let Some(obj) = tool.as_object_mut()
                    && obj.contains_key("cache_control")
                {
                    visit(obj);
                }
            }
        }
        // 2. system blocks (only when serialized as an array of typed blocks)
        if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut) {
            for block in system.iter_mut() {
                if let Some(obj) = block.as_object_mut()
                    && obj.contains_key("cache_control")
                {
                    visit(obj);
                }
            }
        }
        // 3. message content blocks (the trailing breakpoint lives on the last
        //    block of the last message, but scan all for robustness)
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages.iter_mut() {
                if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for block in content.iter_mut() {
                        if let Some(obj) = block.as_object_mut()
                            && obj.contains_key("cache_control")
                        {
                            visit(obj);
                        }
                    }
                }
            }
        }
    }

    /// Enforce Anthropic's hard limit of `MAX_CACHE_CONTROL_MARKERS` cache_control
    /// breakpoints per request. Markers are kept in cache-value priority order
    /// (tools -> system prefix -> trailing message breakpoint); any beyond the
    /// cap are dropped so the request never 400s on excess breakpoints.
    ///
    /// No-op for the common case (<= cap markers), so behavior is identical when
    /// the request is already within bounds.
    fn enforce_cache_control_cap(body: &mut Value) {
        let mut total = 0usize;
        Self::for_each_cache_marker(body, |_| total += 1);
        if total <= MAX_CACHE_CONTROL_MARKERS {
            return;
        }

        let mut seen = 0usize;
        Self::for_each_cache_marker(body, |obj| {
            seen += 1;
            if seen > MAX_CACHE_CONTROL_MARKERS {
                obj.remove("cache_control");
            }
        });

        tracing::warn!(
            requested = total,
            kept = MAX_CACHE_CONTROL_MARKERS,
            dropped = total - MAX_CACHE_CONTROL_MARKERS,
            "anthropic: capped cache_control breakpoints to {MAX_CACHE_CONTROL_MARKERS} \
             (requested {total}); dropped lowest-priority markers to avoid a 400"
        );
    }

    /// Compute a cheap, stable hash of the `cache_control`-marked STABLE PREFIX of
    /// an assembled request body.
    ///
    /// # Why this exists (B3 drift guard)
    ///
    /// Anthropic prompt caching only registers a hit when the bytes preceding a
    /// `cache_control` breakpoint are *byte-identical* to a previous request. If a
    /// timestamp, a `HashMap`-iteration-order leak, or any other non-deterministic
    /// value sneaks into the prefix that is *supposed* to be stable, every cache hit
    /// silently turns into a (paid) cache miss with no error surfaced anywhere.
    ///
    /// This hash lets callers (and the regression test below) assert that identical
    /// logical inputs produce a byte-identical cached prefix. It deliberately hashes
    /// only the prefix segments that actually carry a `cache_control` marker — the
    /// tool definitions, the cached system blocks, and the trailing message
    /// breakpoint's preceding content — in the same descending priority order that
    /// [`Self::for_each_cache_marker`] visits them, so the hash tracks exactly the
    /// bytes Anthropic keys its cache on.
    ///
    /// It is intentionally allocation-light: it folds the relevant `serde_json`
    /// values into a `DefaultHasher` field-by-field rather than re-serializing or
    /// cloning the whole body. The value is only stable *within a single process
    /// run* (`DefaultHasher` is not portable), which is all a within-run drift check
    /// needs. Returns `None` when the request carries no cache markers at all
    /// (caching inactive — nothing to guard).
    fn stable_prefix_hash(body: &Value) -> Option<u64> {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut marked = 0usize;

        // Recursively fold a JSON value in a way that is independent of physical
        // map storage order: object entries are hashed in sorted-key order so the
        // result is identical regardless of how the map happens to iterate. (With
        // `serde_json`'s default `BTreeMap` backing this is already the case, but
        // hashing explicitly by sorted key makes the guard robust even if the
        // `preserve_order` feature is ever enabled upstream.)
        fn fold(value: &Value, hasher: &mut impl Hasher) {
            match value {
                Value::Null => 0u8.hash(hasher),
                Value::Bool(b) => {
                    1u8.hash(hasher);
                    b.hash(hasher);
                }
                Value::Number(n) => {
                    2u8.hash(hasher);
                    n.to_string().hash(hasher);
                }
                Value::String(s) => {
                    3u8.hash(hasher);
                    s.hash(hasher);
                }
                Value::Array(items) => {
                    4u8.hash(hasher);
                    items.len().hash(hasher);
                    for item in items {
                        fold(item, hasher);
                    }
                }
                Value::Object(map) => {
                    5u8.hash(hasher);
                    map.len().hash(hasher);
                    let mut keys: Vec<&String> = map.keys().collect();
                    keys.sort_unstable();
                    for key in keys {
                        key.hash(hasher);
                        fold(&map[key], hasher);
                    }
                }
            }
        }

        // Tools: the model id participates in the cached tool prefix on Anthropic's
        // side, so fold it first, then every tool definition in order. Tools are the
        // largest / highest-reuse cached segment.
        if let Some(model) = body.get("model") {
            fold(model, &mut hasher);
        }
        if let Some(tools) = body.get("tools").and_then(Value::as_array) {
            for tool in tools {
                fold(tool, &mut hasher);
                if tool.get("cache_control").is_some() {
                    marked += 1;
                }
            }
        }

        // System blocks (when serialized as typed blocks). The cached prefix is the
        // ordered run of blocks; the marker sits on every block except the dynamic
        // tail, so the whole array is part of the stable prefix bytes.
        if let Some(system) = body.get("system").and_then(Value::as_array) {
            for block in system {
                fold(block, &mut hasher);
                if block.get("cache_control").is_some() {
                    marked += 1;
                }
            }
        }

        // Trailing message breakpoint: fold the content blocks that carry the
        // marker. The marker lives on the last block of the last message and closes
        // the conversation-prefix cache boundary.
        if let Some(messages) = body.get("messages").and_then(Value::as_array) {
            for message in messages {
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for block in content {
                        if block.get("cache_control").is_some() {
                            fold(block, &mut hasher);
                            marked += 1;
                        }
                    }
                }
            }
        }

        if marked == 0 {
            return None;
        }
        marked.hash(&mut hasher);
        Some(hasher.finish())
    }

    fn build_request(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Value {
        let (_system, mut messages) = conversation.to_anthropic_messages();
        let system_blocks = Self::system_blocks(conversation);

        // Message-level cache breakpoint: mark the last message so the full
        // conversation prefix is cacheable across consecutive turns.
        if Self::has_cache_metadata(conversation) {
            Self::add_message_cache_breakpoint(&mut messages);
        }

        let max_tokens = self
            .config
            .capabilities
            .max_tokens_default
            .unwrap_or(64_000);

        let mut body = json!({
            "model": self.config.model_id,
            "messages": messages,
            "max_tokens": max_tokens
        });

        if let Some(system_value) = Self::serialize_system_blocks(&system_blocks) {
            body["system"] = system_value;
        }

        // Extended thinking. `None` => no `thinking` block (pre-B5 behavior).
        // `Some(tier)` => enable extended thinking with a per-tier budget,
        // clamped strictly below the request's `max_tokens` (Anthropic requires
        // budget_tokens < max_tokens). When thinking is enabled Anthropic also
        // requires `temperature` to be unset/1, so we never set temperature on
        // this path (we don't today), and the tool_choice block below skips
        // emitting a forcing `tool_choice` while thinking is on.
        if let Some(tier) = self.config.reasoning_effort {
            let budget = tier
                .thinking_budget()
                .min(max_tokens.saturating_sub(1))
                .max(1);
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget
            });
        }

        if let Some(serialized_tools) = Self::serialize_tools_for_request(conversation, tools) {
            body["tools"] = serialized_tools;

            let thinking_enabled = body
                .get("thinking")
                .and_then(|thinking| thinking.get("type"))
                .and_then(Value::as_str)
                == Some("enabled");

            if !thinking_enabled {
                match tool_choice.unwrap_or(ToolChoice::Auto) {
                    ToolChoice::Auto => {}
                    ToolChoice::Required => body["tool_choice"] = json!({"type": "any"}),
                    ToolChoice::None => body["tool_choice"] = json!({"type": "none"}),
                }
            }
        }

        if self.config.capabilities.streaming {
            body["stream"] = json!(true);
        }

        // Defensive: Anthropic rejects requests with more than
        // MAX_CACHE_CONTROL_MARKERS cache_control breakpoints with a 400. Drop
        // any excess, keeping the most cache-valuable segments.
        Self::enforce_cache_control_cap(&mut body);

        // B3 drift guard: hash the final cache_control-marked stable prefix and warn
        // if it changed since this instance's previous turn. A changed "stable"
        // prefix means a silent, total prompt-cache miss (pure cost, no error), so
        // it is worth a loud breadcrumb. Cheap (one structural hash + compare, no
        // body clone), non-fatal (never alters the request), and gated to fire only
        // on an *actual* change — the first turn and caching-inactive requests are
        // silent.
        self.check_prefix_drift(&body);

        body
    }

    /// Compare the freshly-assembled stable-prefix hash against this provider
    /// instance's previous request and `warn!` on an unexpected change. See
    /// [`Self::stable_prefix_hash`] for what counts as the stable prefix.
    fn check_prefix_drift(&self, body: &Value) {
        let Some(current) = Self::stable_prefix_hash(body) else {
            return; // no cache markers => nothing to guard
        };
        let mut last = match self.last_prefix_hash.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = *last
            && previous != current
        {
            tracing::warn!(
                previous_hash = previous,
                current_hash = current,
                model = %self.config.model_id,
                "anthropic: cache_control stable prefix changed across consecutive turns; \
                 prompt cache will MISS for this request. A timestamp or non-deterministic \
                 value likely leaked into the cached prefix (system blocks / tool definitions). \
                 This is non-fatal but increases cost — inspect the stable prefix for drift."
            );
        }
        *last = Some(current);
    }

    fn effective_url(&self) -> String {
        format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'))
    }

    fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();

        // Anthropic version header (always required)
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );

        headers
    }
}

// ─── SSE parsing helpers ──────────────────────────────────────────────────────

/// State machine for accumulating a streaming tool use block.
#[derive(Default)]
pub(crate) struct ToolAcc {
    id: String,
    name: String,
    input_json: String,
}

/// Parse a single Anthropic SSE event (event_type + data JSON).
/// Mutates `tool_acc` in place; caller owns it across calls.
pub(crate) fn parse_anthropic_event(
    event_type: &str,
    data: &str,
    tool_acc: &mut Option<ToolAcc>,
    input_tokens: &mut u32,
    cache_read: &mut u32,
    cache_write: &mut u32,
) -> Vec<StreamEvent> {
    let mut events = vec![];

    match event_type {
        "message_start" => {
            // {"type":"message_start","message":{"usage":{"input_tokens":N,
            //  "cache_read_input_tokens":N,"cache_creation_input_tokens":N,...}}}
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(n) = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *input_tokens = n as u32;
                }
                if let Some(n) = v
                    .pointer("/message/usage/cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_read = n as u32;
                }
                if let Some(n) = v
                    .pointer("/message/usage/cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_write = n as u32;
                }
            }
        }

        "content_block_start" => {
            // {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"...","name":"..."}}
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let block_type = v
                    .pointer("/content_block/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match block_type {
                    "tool_use" => {
                        let id = v
                            .pointer("/content_block/id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = v
                            .pointer("/content_block/name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        *tool_acc = Some(ToolAcc {
                            id,
                            name,
                            input_json: String::new(),
                        });
                    }
                    "thinking" => {
                        // Extended thinking block — nothing to accumulate at start.
                    }
                    _ => {}
                }
            }
        }

        "content_block_delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let delta_type = v
                    .pointer("/delta/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                match delta_type {
                    "text_delta" => {
                        let text = v
                            .pointer("/delta/text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !text.is_empty() {
                            events.push(StreamEvent::Delta(ContentBlock::Text { text }));
                        }
                    }
                    "thinking_delta" => {
                        let thinking = v
                            .pointer("/delta/thinking")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !thinking.is_empty() {
                            events.push(StreamEvent::Thinking(thinking));
                        }
                    }
                    "input_json_delta" => {
                        if let Some(acc) = tool_acc.as_mut()
                            && let Some(frag) =
                                v.pointer("/delta/partial_json").and_then(|x| x.as_str())
                        {
                            acc.input_json.push_str(frag);
                        }
                    }
                    _ => {}
                }
            }
        }

        "content_block_stop" => {
            // If we were accumulating a tool use, emit it now
            if let Some(acc) = tool_acc.take() {
                let input = serde_json::from_str(&acc.input_json)
                    .unwrap_or(Value::Object(Default::default()));
                events.push(StreamEvent::Delta(ContentBlock::ToolUse {
                    id: acc.id,
                    name: acc.name,
                    input,
                }));
            }
        }

        "message_delta" => {
            // {"type":"message_delta","usage":{"output_tokens":N,
            //  "cache_read_input_tokens":N,"cache_creation_input_tokens":N}}
            // Anthropic may also restate cache counts here — fold them in if present.
            if let Ok(v) = serde_json::from_str::<Value>(data)
                && let Some(n) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64())
            {
                if let Some(c) = v
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_read = c as u32;
                }
                if let Some(c) = v
                    .pointer("/usage/cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                {
                    *cache_write = c as u32;
                }
                events.push(StreamEvent::Usage(TokenUsage {
                    input: *input_tokens,
                    output: n as u32,
                    cache_read: *cache_read,
                    cache_write: *cache_write,
                    reasoning_output: 0,
                }));
            }
        }

        "message_stop" => {
            events.push(StreamEvent::Done);
        }

        _ => {} // ping, error, etc.
    }

    events
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn config_snapshot(&self) -> Option<ProviderConfig> {
        Some(self.config.clone())
    }

    fn stream<'a>(
        &'a self,
        conversation: &'a Conversation,
        tools: &'a [Value],
        tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let body = self.build_request(conversation, tools, tool_choice);
        let url = self.effective_url();
        let extra_headers = self.extra_headers();

        // For Anthropic, auth is via x-api-key header; we pass NoAuth here and
        // rely on the ApiKeyHeader auth being set in config.auth which is passed through.
        let auth = self.config.auth.clone();

        Box::pin(async move {
            let raw = self.client.stream_sse(&url, body, &auth, extra_headers);

            let out: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(stream! {
                    let mut tool_acc: Option<ToolAcc> = None;
                    let mut input_tokens: u32 = 0;
                    let mut cache_read: u32 = 0;
                    let mut cache_write: u32 = 0;

                    // Anthropic SSE uses event: / data: pairs
                    // Our client currently yields only data: lines.
                    // We need to track event: lines too. Since ApiClient only yields data lines,
                    // we handle this by parsing event type from the data itself for Anthropic.
                    // The data JSON always has a "type" field.
                    let mut raw_stream = raw;
                    while let Some(result) = raw_stream.next().await {
                        match result {
                            Err(e) => { yield Err(e); return; }
                            Ok(line) => {
                                // Anthropic data lines contain the event type in the JSON
                                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                                    let event_type = v["type"].as_str().unwrap_or("").to_string();
                                    for event in parse_anthropic_event(&event_type, &line, &mut tool_acc, &mut input_tokens, &mut cache_read, &mut cache_write) {
                                        yield Ok(event);
                                    }
                                }
                            }
                        }
                    }
                });
            Ok(out)
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Conversation, Message};
    use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig};
    use axum::{Router, routing::post};
    use futures::TryStreamExt;

    fn spawn_sse_server(status: u16, body: &'static str) -> String {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind local tcp listener");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("set nonblocking");

        let rt = tokio::runtime::Handle::current();
        rt.spawn(async move {
            let app = Router::new().route(
                "/v1/messages",
                post(move |_req: axum::extract::Request| async move {
                    (
                        axum::http::StatusCode::from_u16(status).expect("status"),
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }),
            );

            let tokio_listener =
                tokio::net::TcpListener::from_std(listener).expect("convert to tokio listener");
            axum::serve(tokio_listener, app).await.ok();
        });

        format!("http://{}:{}", addr.ip(), addr.port())
    }

    fn test_anthropic_config() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://example.com".to_string(),
            auth: AuthMethod::NoAuth,
            format_family: FormatFamily::Anthropic,
            model_id: "claude-3-5-sonnet".to_string(),
            context_window: 200_000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: std::collections::HashMap::new(),
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: Some(64_000),
            },
            reasoning_effort: None,
        }
    }

    fn test_provider() -> AnthropicProvider {
        AnthropicProvider::new(test_anthropic_config())
    }

    #[test]
    fn test_message_start_extracts_input_tokens() {
        let data = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let mut cache_read = 0u32;
        let mut cache_write = 0u32;
        let events = parse_anthropic_event(
            "message_start",
            data,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert!(events.is_empty());
        assert_eq!(input_tokens, 25);
    }

    #[test]
    fn test_message_start_extracts_cache_tokens() {
        let data = r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":1000,"cache_creation_input_tokens":40}}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let mut cache_read = 0u32;
        let mut cache_write = 0u32;
        parse_anthropic_event(
            "message_start",
            data,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert_eq!(input_tokens, 25);
        assert_eq!(cache_read, 1000);
        assert_eq!(cache_write, 40);
    }

    #[test]
    fn test_text_delta_event() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let mut cache_read = 0u32;
        let mut cache_write = 0u32;
        let events = parse_anthropic_event(
            "content_block_delta",
            data,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "Hello"),
            _ => panic!("expected text delta"),
        }
    }

    #[test]
    fn test_tool_use_accumulation() {
        let mut acc = None;
        let mut input_tokens = 0u32;
        let mut cache_read = 0u32;
        let mut cache_write = 0u32;

        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"shell"}}"#;
        let e1 = parse_anthropic_event(
            "content_block_start",
            start,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert!(e1.is_empty());
        assert!(acc.is_some());

        let frag1 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"l"}}"#;
        parse_anthropic_event(
            "content_block_delta",
            frag1,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );

        let frag2 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"s\",\"dir\":\"/tmp\"}"}}"#;
        parse_anthropic_event(
            "content_block_delta",
            frag2,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );

        let stop = r#"{"type":"content_block_stop","index":0}"#;
        let events = parse_anthropic_event(
            "content_block_stop",
            stop,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id.as_str(), "toolu_01");
                assert_eq!(name.as_str(), "shell");
                assert_eq!(input["cmd"].as_str(), Some("ls"));
                assert_eq!(input["dir"].as_str(), Some("/tmp"));
            }
            _ => panic!("expected tool use"),
        }
        assert!(acc.is_none());
    }

    #[test]
    fn test_message_delta_emits_usage() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let mut acc = None;
        let mut input_tokens = 10u32;
        let mut cache_read = 3u32;
        let mut cache_write = 7u32;
        let events = parse_anthropic_event(
            "message_delta",
            data,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 10);
                assert_eq!(u.output, 42);
                // Cache counts carried from message_start are folded into usage.
                assert_eq!(u.cache_read, 3);
                assert_eq!(u.cache_write, 7);
            }
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn test_message_stop_emits_done() {
        let data = r#"{"type":"message_stop"}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let mut cache_read = 0u32;
        let mut cache_write = 0u32;
        let events = parse_anthropic_event(
            "message_stop",
            data,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Done));
    }

    #[test]
    fn test_build_request_always_populates_system_field() {
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system("system prompt"));
        conv.push(crate::message::Message::user("first user"));
        conv.push(crate::message::Message::assistant("first assistant"));
        conv.push(crate::message::Message::user("second user"));

        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["system"], "system prompt");
        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "first user");
    }

    #[test]
    fn test_system_blocks_consume_explicit_stable_prefix_metadata_contract() {
        let mut conv = Conversation::default();
        conv.push(crate::message::Message {
            role: crate::message::Role::System,
            content: vec![
                ContentBlock::text("base prompt"),
                ContentBlock::text("project context"),
                ContentBlock::text("repo map"),
                ContentBlock::text("dynamic tail"),
            ],
            metadata: Some(crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: {
                        "kind": ANTHROPIC_STABLE_PREFIX_KIND,
                    }
                })),
            }),
        });
        conv.push(crate::message::Message::user("hello"));

        let blocks = AnthropicProvider::system_blocks(&conv);
        assert_eq!(blocks.len(), 4);
        assert_eq!(
            blocks[0].cache_control,
            Some(json!({"type": "ephemeral", "kind": ANTHROPIC_STABLE_PREFIX_KIND}))
        );
        assert_eq!(
            blocks[1].cache_control,
            Some(json!({"type": "ephemeral", "kind": ANTHROPIC_STABLE_PREFIX_KIND}))
        );
        assert_eq!(
            blocks[2].cache_control,
            Some(json!({"type": "ephemeral", "kind": ANTHROPIC_STABLE_PREFIX_KIND}))
        );
        assert_eq!(blocks[3].cache_control, None);
    }

    #[test]
    fn test_build_request_preserves_separate_system_blocks_with_cache_control() {
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system_with_metadata(
            "base prompt",
            crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
                    }
                })),
            },
        ));
        conv.messages[0].content.push(ContentBlock::Text {
            text: "repo map".to_string(),
        });
        conv.push(crate::message::Message::user("hello"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "input_schema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let system = req["system"].as_array().expect("system block array");
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], "base prompt");
        assert_eq!(system[1]["text"], "repo map");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            system[0]["cache_control"]["kind"],
            ANTHROPIC_STABLE_PREFIX_KIND
        );
        assert!(system[1].get("cache_control").is_none());
        assert_eq!(req["tools"][0]["name"], "shell");
        assert_eq!(
            req["tools"][0]["cache_control"]["kind"],
            ANTHROPIC_STABLE_PREFIX_KIND
        );
    }

    #[test]
    fn test_content_block_delta_input_json_without_active_tool_is_ignored() {
        let data = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}"#;
        let mut acc = None;
        let mut input_tokens = 0u32;
        let mut cache_read = 0u32;
        let mut cache_write = 0u32;
        let events = parse_anthropic_event(
            "content_block_delta",
            data,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        );
        assert!(events.is_empty());
        assert!(acc.is_none());
    }

    #[tokio::test]
    async fn test_stream_uses_payload_type_over_sse_event_name() {
        let body = concat!(
            "event: nope\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
            "event: wrong-name\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello from payload\"}}\n\n",
            "event: definitely-not-message-delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
            "event: not-message-stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let mut config = test_anthropic_config();
        config.base_url = spawn_sse_server(200, body);
        let provider = AnthropicProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let events = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream")
            .try_collect::<Vec<_>>()
            .await
            .expect("collect events");

        assert_eq!(events.len(), 3);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => {
                assert_eq!(text, "Hello from payload")
            }
            _ => panic!("expected text delta"),
        }
        match &events[1] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, 7);
                assert_eq!(u.output, 9);
            }
            _ => panic!("expected usage"),
        }
        assert!(matches!(events[2], StreamEvent::Done));
    }

    #[tokio::test]
    async fn test_streamed_error_event_is_ignored_but_http_error_shape_surfaces() {
        let body = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"try again later\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let mut config = test_anthropic_config();
        config.base_url = spawn_sse_server(200, body);
        let provider = AnthropicProvider::new(config);
        let mut conv = Conversation::new();
        conv.push(Message::user("Hello"));

        let events = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream")
            .try_collect::<Vec<_>>()
            .await
            .expect("collect events");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Done));

        let error_body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let mut error_config = test_anthropic_config();
        error_config.base_url = spawn_sse_server(401, error_body);
        let provider = AnthropicProvider::new(error_config);
        let err = provider
            .stream(&conv, &[], None)
            .await
            .expect("stream")
            .try_collect::<Vec<_>>()
            .await
            .expect_err("expected anthropic http error");
        let err_text = err.to_string();
        assert!(err_text.contains("provider API error 401 Unauthorized"));
        assert!(err_text.contains("authentication_error"));
        assert!(err_text.contains("invalid x-api-key"));
    }

    #[test]
    fn test_build_request_sets_required_tool_choice_when_tools_present() {
        let provider = test_provider();
        let mut conv = Conversation::new();
        conv.push(crate::message::Message::user("Hello"));
        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "input_schema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
        assert_eq!(req["tool_choice"]["type"], "any");
    }

    // ─── Empty-segment handling tests ─────────────────────────────────────────

    #[test]
    fn test_system_blocks_skips_empty_and_whitespace_content() {
        let mut conv = Conversation::default();
        conv.push(crate::message::Message {
            role: djinn_core::message::Role::System,
            content: vec![
                ContentBlock::Text {
                    text: "base prompt".to_string(),
                },
                ContentBlock::Text {
                    text: "".to_string(),
                },
                ContentBlock::Text {
                    text: "   \n  ".to_string(),
                },
                ContentBlock::Text {
                    text: "dynamic tail".to_string(),
                },
            ],
            metadata: None,
        });
        conv.push(crate::message::Message::user("hello"));

        let blocks = AnthropicProvider::system_blocks(&conv);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "base prompt");
        assert_eq!(blocks[1].text, "dynamic tail");
    }

    #[test]
    fn test_system_blocks_empty_conversation_produces_no_blocks() {
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::user("hello"));

        let blocks = AnthropicProvider::system_blocks(&conv);
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_serialize_system_blocks_returns_none_for_empty() {
        let result = AnthropicProvider::serialize_system_blocks(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_serialize_system_blocks_single_no_cache() {
        let blocks = vec![AnthropicSystemBlock {
            text: "hello".to_string(),
            cache_control: None,
        }];
        let result = AnthropicProvider::serialize_system_blocks(&blocks);
        assert_eq!(result, Some(Value::String("hello".to_string())));
    }

    #[test]
    fn test_build_request_no_system_field_when_no_system_message() {
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::user("hello"));

        let req = provider.build_request(&conv, &[], None);
        assert!(
            req.get("system").is_none(),
            "system field should be absent when there are no system blocks"
        );
    }

    #[test]
    fn test_build_request_with_all_empty_system_content_omits_system() {
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message {
            role: djinn_core::message::Role::System,
            content: vec![
                ContentBlock::Text {
                    text: "".to_string(),
                },
                ContentBlock::Text {
                    text: "   ".to_string(),
                },
            ],
            metadata: None,
        });
        conv.push(crate::message::Message::user("hello"));

        let req = provider.build_request(&conv, &[], None);
        assert!(
            req.get("system").is_none(),
            "system field should be absent when all system content blocks are empty"
        );
    }

    // ─── B5: reasoning-effort -> thinking block ─────────────────────────────

    #[test]
    fn test_reasoning_effort_none_omits_thinking_block() {
        // None must preserve pre-B5 behavior: no `thinking` block at all.
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::user("hello"));
        let req = provider.build_request(&conv, &[], None);
        assert!(
            req.get("thinking").is_none(),
            "thinking block must be absent when reasoning_effort is None"
        );
    }

    #[test]
    fn test_reasoning_effort_high_enables_thinking() {
        use crate::provider::ReasoningEffort;
        let mut config = test_anthropic_config();
        config.reasoning_effort = Some(ReasoningEffort::High);
        let provider = AnthropicProvider::new(config);
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::user("hello"));
        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["thinking"]["type"], "enabled");
        // High budget (24000) is below max_tokens (64000), so it passes through.
        assert_eq!(req["thinking"]["budget_tokens"], 24_000);
    }

    #[test]
    fn test_reasoning_effort_budget_clamped_below_max_tokens() {
        use crate::provider::{ProviderCapabilities, ReasoningEffort};
        let mut config = test_anthropic_config();
        // Force a tiny output limit so the tier budget must be clamped.
        config.capabilities = ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(2_000),
        };
        config.reasoning_effort = Some(ReasoningEffort::High);
        let provider = AnthropicProvider::new(config);
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::user("hello"));
        let req = provider.build_request(&conv, &[], None);
        assert_eq!(req["thinking"]["type"], "enabled");
        // Clamped to max_tokens - 1.
        assert_eq!(req["thinking"]["budget_tokens"], 1_999);
    }

    #[test]
    fn test_reasoning_effort_enabled_skips_forced_tool_choice() {
        use crate::provider::ReasoningEffort;
        let mut config = test_anthropic_config();
        config.reasoning_effort = Some(ReasoningEffort::Medium);
        let provider = AnthropicProvider::new(config);
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::user("hello"));
        let tools = vec![json!({
            "name": "do_thing",
            "description": "does a thing",
            "input_schema": {"type": "object", "properties": {}}
        })];
        let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
        // With thinking enabled, the forcing tool_choice must NOT be emitted.
        assert!(
            req.get("tool_choice").is_none(),
            "tool_choice must be omitted when thinking is enabled"
        );
        assert_eq!(req["thinking"]["type"], "enabled");
    }

    #[test]
    fn test_cache_control_correct_after_empty_block_filtering() {
        let mut conv = Conversation::default();
        conv.push(crate::message::Message {
            role: djinn_core::message::Role::System,
            content: vec![
                ContentBlock::Text {
                    text: "base prompt".to_string(),
                },
                ContentBlock::Text {
                    text: "".to_string(),
                },
                ContentBlock::Text {
                    text: "tools".to_string(),
                },
                ContentBlock::Text {
                    text: "   ".to_string(),
                },
                ContentBlock::Text {
                    text: "dynamic tail".to_string(),
                },
            ],
            metadata: Some(crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            }),
        });
        conv.push(crate::message::Message::user("hello"));

        let blocks = AnthropicProvider::system_blocks(&conv);
        // After filtering: ["base prompt", "tools", "dynamic tail"]
        assert_eq!(blocks.len(), 3);
        // First two should have cache_control, last should not
        assert!(
            blocks[0].cache_control.is_some(),
            "first block should have cache_control"
        );
        assert!(
            blocks[1].cache_control.is_some(),
            "second block should have cache_control"
        );
        assert!(
            blocks[2].cache_control.is_none(),
            "last block should NOT have cache_control"
        );
    }

    #[test]
    fn test_cache_control_when_trailing_empty_blocks_are_filtered() {
        let mut conv = Conversation::default();
        conv.push(crate::message::Message {
            role: djinn_core::message::Role::System,
            content: vec![
                ContentBlock::Text {
                    text: "base prompt".to_string(),
                },
                ContentBlock::Text {
                    text: "cached segment".to_string(),
                },
                ContentBlock::Text {
                    text: "".to_string(),
                },
            ],
            metadata: Some(crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            }),
        });
        conv.push(crate::message::Message::user("hello"));

        let blocks = AnthropicProvider::system_blocks(&conv);
        // After filtering: ["base prompt", "cached segment"]
        assert_eq!(blocks.len(), 2);
        assert!(
            blocks[0].cache_control.is_some(),
            "first block should have cache_control"
        );
        assert!(
            blocks[1].cache_control.is_none(),
            "last non-empty block should NOT have cache_control (it is now the tail)"
        );
    }

    #[test]
    fn test_populated_segments_unchanged() {
        // Verify that the existing behavior for fully-populated segments is preserved
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system_with_metadata(
            "base prompt",
            crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            },
        ));
        conv.messages[0].content.push(ContentBlock::Text {
            text: "repo map".to_string(),
        });
        conv.push(crate::message::Message::user("hello"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "input_schema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let system = req["system"].as_array().expect("system block array");
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], "base prompt");
        assert_eq!(system[1]["text"], "repo map");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(system[0]["cache_control"]["kind"], "stable_prefix");
        assert!(system[1].get("cache_control").is_none());
        assert_eq!(req["tools"][0]["cache_control"]["kind"], "stable_prefix");
    }

    // ─── End-to-end prompt assembly → Anthropic request coverage ──────────────

    /// Build a system message using the current chat-layer production contract:
    /// trim the base prompt, keep project context as a stable block,
    /// collapse dynamic client/task text into a trailing block, and attach
    /// Anthropic cache metadata only for Anthropic models.
    fn build_system_message_for_test(
        base_prompt: &str,
        project_context: Option<&str>,
        client_system: Option<&str>,
        is_anthropic: bool,
    ) -> Message {
        let mut content = vec![ContentBlock::text(base_prompt.trim())];
        if let Some(project_context) = project_context.filter(|s| !s.trim().is_empty()) {
            content.push(ContentBlock::text(project_context));
        }
        if let Some(client_system) = client_system.filter(|s| !s.trim().is_empty()) {
            content.push(ContentBlock::text(client_system));
        }

        let metadata = is_anthropic.then(|| crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        });

        Message {
            role: crate::message::Role::System,
            content,
            metadata,
        }
    }

    /// E2E: with repo map present, Anthropic keeps tool definitions in the
    /// dedicated request-level `tools` block while preserving the system block
    /// ordering from `chat.rs` (base -> project context -> repo map -> dynamic
    /// tail). Stable-prefix `cache_control` appears on the stable system prefix
    /// and on the first tool-definition entry, but not on the dynamic tail.
    #[test]
    fn e2e_system_blocks_ordered_with_cache_control() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";
        let project_context = "## Project Context\nworkspace: demo";
        let client = "Be concise.";

        let sys_msg =
            build_system_message_for_test(base, Some(project_context), Some(client), true);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("What does this project do?"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run a shell command",
            "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let system = req["system"]
            .as_array()
            .expect("system should be an array when cache_control is present");

        assert_eq!(system.len(), 3, "expected 3 system blocks");
        assert_eq!(system[0]["text"], base.trim());
        assert_eq!(system[1]["text"], project_context);
        assert_eq!(system[2]["text"], client);

        for stable_block in &system[..2] {
            assert_eq!(stable_block["cache_control"]["type"], "ephemeral");
            assert_eq!(stable_block["cache_control"]["kind"], "stable_prefix");
        }
        assert!(
            system[2].get("cache_control").is_none(),
            "dynamic tail block must not have cache_control"
        );
        assert_eq!(req["tools"][0]["cache_control"]["kind"], "stable_prefix");
    }

    /// E2E: without tools or dynamic context, a single non-cacheable
    /// system block collapses to a plain string (no array, no cache_control).
    #[test]
    fn e2e_single_block_no_cache_control() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";

        let sys_msg = build_system_message_for_test(base, None, None, false);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);

        assert!(
            req["system"].is_string(),
            "single-block system without cache_control should serialize as a plain string"
        );
        assert_eq!(req["system"], base.trim());
    }

    /// E2E: Anthropic model with base prompt only (no optional contexts) still
    /// serializes as a plain string because the only block is also the dynamic
    /// cache boundary and therefore receives no `cache_control`.
    #[test]
    fn e2e_anthropic_base_only_with_cache_metadata_formats_as_single_block() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";

        let sys_msg = build_system_message_for_test(base, None, None, true);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("Hello"));

        let req = provider.build_request(&conv, &[], None);

        assert!(
            req["system"].is_string(),
            "single-block anthropic system should still be a plain string \
             when cache_control is absent on the only block"
        );
        assert_eq!(req["system"], base.trim());
    }

    /// E2E: session with request-level tools verifies that Anthropic
    /// keeps the stable system prefix ordered as base -> project context,
    /// preserves the uncached dynamic tail, and still emits the separate
    /// request `tools` array unchanged.
    #[test]
    fn e2e_tools_preserves_both_system_and_tools() {
        let provider = test_provider();
        let base = "You are a helpful assistant.";
        let project_context = "## Tool Definitions\nshell(cmd: string)";

        let sys_msg =
            build_system_message_for_test(base, Some(project_context), Some("be brief"), true);

        let mut conv = Conversation::new();
        conv.push(sys_msg);
        conv.push(Message::user("List files"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run a shell command",
            "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
        })];

        let req = provider.build_request(&conv, &tools, None);
        let system = req["system"]
            .as_array()
            .expect("system should be array with cache_control");
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], base.trim());
        assert_eq!(system[1]["text"], project_context);
        assert_eq!(system[2]["text"], "be brief");
        assert_eq!(system[0]["cache_control"]["kind"], "stable_prefix");
        assert_eq!(system[1]["cache_control"]["kind"], "stable_prefix");
        assert!(system[2].get("cache_control").is_none());

        let req_tools = req["tools"].as_array().expect("tools array");
        assert_eq!(req_tools.len(), 1);
        assert_eq!(req_tools[0]["name"], "shell");
    }

    // ─── B2: cache_control breakpoint cap (Anthropic max 4) ───────────────────

    /// Count every `cache_control` marker present across tools, system blocks,
    /// and message content in a serialized request body.
    fn count_cache_markers(body: &Value) -> usize {
        let mut count = 0;
        if let Some(tools) = body.get("tools").and_then(Value::as_array) {
            count += tools
                .iter()
                .filter(|t| t.get("cache_control").is_some())
                .count();
        }
        if let Some(system) = body.get("system").and_then(Value::as_array) {
            count += system
                .iter()
                .filter(|b| b.get("cache_control").is_some())
                .count();
        }
        if let Some(messages) = body.get("messages").and_then(Value::as_array) {
            for message in messages {
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    count += content
                        .iter()
                        .filter(|b| b.get("cache_control").is_some())
                        .count();
                }
            }
        }
        count
    }

    #[test]
    fn test_cache_control_markers_capped_at_four() {
        let provider = test_provider();
        let mut conv = Conversation::default();
        // Six non-empty system text blocks with cache metadata. system_blocks
        // marks all-but-last (5 cached), the request marks the first tool (1),
        // and add_message_cache_breakpoint marks the last message (1): 7 raw
        // markers, well over the cap of 4.
        conv.push(crate::message::Message {
            role: djinn_core::message::Role::System,
            content: vec![
                ContentBlock::text("base prompt"),
                ContentBlock::text("project context"),
                ContentBlock::text("repo map"),
                ContentBlock::text("conventions"),
                ContentBlock::text("more stable context"),
                ContentBlock::text("dynamic tail"),
            ],
            metadata: Some(crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
                    }
                })),
            }),
        });
        conv.push(crate::message::Message::user("hello"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "input_schema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, None);

        // Hard cap enforced.
        let total = count_cache_markers(&req);
        assert!(
            total <= 4,
            "expected at most 4 cache_control markers, got {total}"
        );
        assert_eq!(total, 4, "should keep exactly 4 markers when over the cap");

        // Highest-priority segments keep their markers: the tool definition (1)
        // and the earliest system blocks (priority after tools).
        assert_eq!(
            req["tools"][0]["cache_control"]["kind"], ANTHROPIC_STABLE_PREFIX_KIND,
            "the tool definition is the highest-priority cache segment and must keep its marker"
        );
        let system = req["system"].as_array().expect("system array");
        assert!(
            system[0].get("cache_control").is_some(),
            "earliest system block must keep its marker"
        );
        assert!(
            system[1].get("cache_control").is_some(),
            "second system block must keep its marker"
        );
        assert!(
            system[2].get("cache_control").is_some(),
            "third system block must keep its marker"
        );
        // Total kept = 1 (tool) + 3 (system) = 4; everything else dropped,
        // including the trailing message breakpoint (lowest priority).
        let messages = req["messages"].as_array().expect("messages array");
        let last = messages.last().expect("last message");
        let last_block = last["content"].as_array().expect("content").last().unwrap();
        assert!(
            last_block.get("cache_control").is_none(),
            "lowest-priority message breakpoint must be dropped past the cap"
        );
    }

    #[test]
    fn test_cache_control_under_cap_is_unchanged() {
        // <= 4 markers: enforcement is a no-op (no regression for the common case).
        let provider = test_provider();
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system_with_metadata(
            "base prompt",
            crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
                    }
                })),
            },
        ));
        conv.messages[0].content.push(ContentBlock::Text {
            text: "repo map".to_string(),
        });
        conv.push(crate::message::Message::user("hello"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "input_schema": {"type": "object"}
        })];

        let req = provider.build_request(&conv, &tools, None);
        // tool(1) + system stable prefix(1) + message breakpoint(1) = 3 <= 4.
        let total = count_cache_markers(&req);
        assert!(total <= 4, "expected <= 4 markers, got {total}");
        // Tool and first system block markers preserved exactly.
        assert_eq!(
            req["tools"][0]["cache_control"]["kind"],
            ANTHROPIC_STABLE_PREFIX_KIND
        );
        assert_eq!(req["system"][0]["cache_control"]["type"], "ephemeral");
    }

    // ─── B3: cache stable-prefix drift guard ──────────────────────────────────

    /// Build a representative cache-enabled conversation + tools used by the B3
    /// drift-guard tests: a stable base prompt, a stable project-context block, a
    /// dynamic trailing block, plus a tool definition. Mirrors the production
    /// chat-layer contract so the cache_control markers land on tools + system
    /// prefix + trailing message breakpoint.
    fn drift_guard_fixture() -> (Conversation, Vec<Value>) {
        let mut conv = Conversation::default();
        conv.push(crate::message::Message::system_with_metadata(
            "base prompt",
            crate::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            },
        ));
        conv.messages[0].content.push(ContentBlock::Text {
            text: "project context / repo map".to_string(),
        });
        conv.messages[0].content.push(ContentBlock::Text {
            text: "dynamic tail".to_string(),
        });
        conv.push(crate::message::Message::user("hello"));

        let tools = vec![json!({
            "name": "shell",
            "description": "Run shell",
            "input_schema": {"type": "object"}
        })];
        (conv, tools)
    }

    /// Determinism: identical logical inputs must produce a byte-identical cached
    /// prefix, hence an identical stable-prefix hash, across two independent
    /// builds. This is the core invariant prompt caching depends on — if it ever
    /// fails, some non-deterministic value (timestamp, map-iteration order, …) has
    /// leaked into the cached prefix and every cache hit silently becomes a miss.
    #[test]
    fn test_stable_prefix_hash_is_deterministic() {
        let provider = test_provider();
        let (conv, tools) = drift_guard_fixture();

        let body_a = provider.build_request(&conv, &tools, None);
        let body_b = provider.build_request(&conv, &tools, None);

        let hash_a = AnthropicProvider::stable_prefix_hash(&body_a)
            .expect("cache markers present => hash should exist");
        let hash_b = AnthropicProvider::stable_prefix_hash(&body_b)
            .expect("cache markers present => hash should exist");

        assert_eq!(
            hash_a, hash_b,
            "stable cache prefix must hash identically across two builds of the same inputs"
        );
        // And the full serialized prefix bytes must match too (stronger than the
        // hash, and guards against an accidental hash collision masking real drift).
        assert_eq!(
            serde_json::to_string(&body_a["system"]).unwrap(),
            serde_json::to_string(&body_b["system"]).unwrap(),
            "serialized system prefix must be byte-identical"
        );
        assert_eq!(
            serde_json::to_string(&body_a["tools"]).unwrap(),
            serde_json::to_string(&body_b["tools"]).unwrap(),
            "serialized tool prefix must be byte-identical"
        );
    }

    /// The hash must be sensitive to actual changes in the cached prefix: a
    /// perturbed stable block (here, a mutated project-context string) must yield a
    /// different hash. Without this, the guard would never detect real drift.
    #[test]
    fn test_stable_prefix_hash_detects_perturbed_prefix() {
        let provider = test_provider();
        let (conv, tools) = drift_guard_fixture();
        let baseline = provider.build_request(&conv, &tools, None);
        let baseline_hash = AnthropicProvider::stable_prefix_hash(&baseline).unwrap();

        // Perturb a STABLE (cache_control-marked) system block, simulating a
        // timestamp / non-deterministic leak into the supposedly-stable prefix.
        let mut perturbed = conv.clone();
        perturbed.messages[0].content[1] = ContentBlock::Text {
            text: "project context / repo map @ 2026-06-01T12:00:00Z".to_string(),
        };
        let perturbed_body = provider.build_request(&perturbed, &tools, None);
        let perturbed_hash = AnthropicProvider::stable_prefix_hash(&perturbed_body).unwrap();

        assert_ne!(
            baseline_hash, perturbed_hash,
            "a mutated stable-prefix block must change the stable-prefix hash"
        );
    }

    /// A change confined to the DYNAMIC tail (the trailing message after the
    /// breakpoint) must NOT change the stable-prefix hash — otherwise the guard
    /// would warn on every legitimately-changing turn and become noise. Here the
    /// trailing breakpoint marker sits on the last *user* message content; the
    /// stable prefix is tools + system blocks, which are unchanged.
    #[test]
    fn test_stable_prefix_hash_ignores_dynamic_tail_changes() {
        let provider = test_provider();
        let (conv, tools) = drift_guard_fixture();
        let body_a = provider.build_request(&conv, &tools, None);

        let mut conv_b = conv.clone();
        // The last message is the user turn; its content is the dynamic tail and is
        // not part of the cached system/tool prefix.
        conv_b.push(crate::message::Message::user(
            "a different follow-up question",
        ));
        let body_b = provider.build_request(&conv_b, &tools, None);

        // The system + tool stable prefix is byte-identical, so those serialized
        // segments must match even though the conversation tail differs.
        assert_eq!(
            serde_json::to_string(&body_a["system"]).unwrap(),
            serde_json::to_string(&body_b["system"]).unwrap(),
            "system prefix must be unaffected by a dynamic-tail change"
        );
        assert_eq!(
            serde_json::to_string(&body_a["tools"]).unwrap(),
            serde_json::to_string(&body_b["tools"]).unwrap(),
            "tool prefix must be unaffected by a dynamic-tail change"
        );
    }

    /// The hash folds objects in sorted-key order, so it is independent of physical
    /// map storage order even if `serde_json`'s `preserve_order` feature is ever
    /// enabled. Build the same object with keys inserted in two different orders and
    /// assert the fold produces the same hash.
    #[test]
    fn test_stable_prefix_hash_is_key_order_independent() {
        // Two tool arrays with the same logical content but different key insertion
        // order. With default serde_json (BTreeMap) these already serialize the
        // same, but the explicit sorted-key fold makes the guarantee robust.
        let body_a = json!({
            "model": "claude-3-5-sonnet",
            "tools": [{
                "cache_control": {"type": "ephemeral", "kind": "stable_prefix"},
                "name": "shell",
                "description": "Run shell"
            }]
        });
        let body_b = json!({
            "model": "claude-3-5-sonnet",
            "tools": [{
                "description": "Run shell",
                "name": "shell",
                "cache_control": {"kind": "stable_prefix", "type": "ephemeral"}
            }]
        });
        assert_eq!(
            AnthropicProvider::stable_prefix_hash(&body_a),
            AnthropicProvider::stable_prefix_hash(&body_b),
            "stable-prefix hash must be independent of object key order"
        );
    }

    /// No cache markers => no prefix to guard => `None` (guard stays silent).
    #[test]
    fn test_stable_prefix_hash_none_when_no_markers() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "system": "plain string system, no cache_control",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        assert!(
            AnthropicProvider::stable_prefix_hash(&body).is_none(),
            "a request with no cache_control markers has no stable prefix to hash"
        );
    }
}
