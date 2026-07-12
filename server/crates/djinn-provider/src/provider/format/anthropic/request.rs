use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::message::{CacheBreakpoint, ContentBlock, Conversation};
use crate::provider::client::ApiClient;
use crate::provider::{ProviderConfig, ToolChoice, ToolSchemaCompat};

use super::cache::{ANTHROPIC_CACHE_BREAKPOINT_KEY, MAX_CACHE_CONTROL_MARKERS};
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AnthropicSystemBlock {
    pub(super) text: String,
    pub(super) cache_control: Option<Value>,
}

pub struct AnthropicProvider {
    pub(super) config: ProviderConfig,
    pub(crate) client: ApiClient,
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
            // `CacheBreakpoint.kind` is an internal annotation and must not reach
            // the wire: the API object is exactly `{"type":"ephemeral"}`, and
            // Anthropic-compatible vendors (MiniMax, GLM) may reject unknown keys
            // inside `cache_control`.
            .map(|_| json!({"type": "ephemeral"}))
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
    pub(super) fn system_blocks(conversation: &Conversation) -> Vec<AnthropicSystemBlock> {
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

    pub(super) fn serialize_system_blocks(blocks: &[AnthropicSystemBlock]) -> Option<Value> {
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

    /// Serialize a shared content block using Anthropic's native wire schema.
    ///
    /// Shared `Conversation::to_anthropic_messages` deliberately skips
    /// provider-private state so non-Anthropic callers cannot accidentally
    /// replay it. Anthropic assistant history is the exception: signed and
    /// redacted thinking are required to continue an extended-thinking turn.
    /// Unsigned thinking cannot satisfy Anthropic's replay contract, so it is
    /// omitted rather than turned into an empty text placeholder.
    fn serialize_content_block(block: &ContentBlock, assistant_replay: bool) -> Option<Value> {
        match block {
            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "type": "tool_use", "id": id, "name": name, "input": input,
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content.iter()
                    .filter_map(|block| Self::serialize_content_block(block, false))
                    .collect::<Vec<_>>(),
                "is_error": is_error,
            })),
            ContentBlock::Image { media_type, data } => Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data}
            })),
            ContentBlock::Document {
                media_type,
                data,
                filename,
            } => {
                let mut document = json!({
                    "type": "document",
                    "source": {"type": "base64", "media_type": media_type, "data": data}
                });
                if let Some(filename) = filename {
                    document["title"] = json!(filename);
                }
                Some(document)
            }
            ContentBlock::Thinking {
                thinking,
                signature: Some(signature),
            } if assistant_replay && !signature.is_empty() => Some(json!({
                "type": "thinking", "thinking": thinking, "signature": signature,
            })),
            ContentBlock::RedactedThinking { data } if assistant_replay => {
                Some(json!({"type": "redacted_thinking", "data": data}))
            }
            ContentBlock::Unknown {
                content_type,
                extra,
            } if assistant_replay => {
                // Insert opaque fields first so Djinn's owned discriminant cannot
                // be overridden by a persisted/foreign `extra.type` value.
                let mut object = extra.clone();
                object.insert("type".to_string(), json!(content_type));
                Some(Value::Object(object))
            }
            ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Unknown { .. }
            | ContentBlock::OpenAIReasoning { .. } => None,
        }
    }

    /// Serialize messages locally so Anthropic assistant replay can retain its
    /// signed/provider-owned blocks without altering generic serializer behavior.
    fn serialize_messages(conversation: &Conversation) -> Vec<Value> {
        conversation
            .messages
            .iter()
            .filter_map(|message| {
                let (role, assistant_replay) = match message.role {
                    djinn_core::message::Role::System => return None,
                    djinn_core::message::Role::User => ("user", false),
                    djinn_core::message::Role::Assistant => ("assistant", true),
                };
                let content = message
                    .content
                    .iter()
                    .filter_map(|block| Self::serialize_content_block(block, assistant_replay))
                    .collect::<Vec<_>>();
                Some(json!({"role": role, "content": content}))
            })
            .collect()
    }

    fn tool_definition_cache_control(conversation: &Conversation) -> Option<Value> {
        conversation
            .messages
            .iter()
            .find(|message| message.role == djinn_core::message::Role::System)
            .and_then(Self::maybe_cache_control)
    }

    fn serialize_tools_for_request(
        tools: &[Value],
        cache_control: Option<&Value>,
        compat: Option<ToolSchemaCompat>,
    ) -> Option<Value> {
        if tools.is_empty() {
            return None;
        }

        // The breakpoint marks the END of a cacheable prefix, so it belongs on
        // the LAST tool definition — marking the first would cache only that
        // single tool.
        let last = tools.len() - 1;
        Some(Value::Array(
            tools
                .iter()
                .enumerate()
                .map(|(index, tool)| {
                    let mut tool_obj = super::convert_tool(tool, compat);
                    if index == last
                        && let Some(cache_control) = cache_control
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

    /// Compute a cheap, stable hash of the genuinely-stable, `cache_control`-marked
    /// prefix segments of an assembled request body.
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
    /// logical inputs produce a byte-identical cached prefix. It hashes only the
    /// segments that are supposed to be *stable across consecutive turns*: the model
    /// id, the `cache_control`-marked tool definitions, and the `cache_control`-marked
    /// system blocks.
    ///
    /// # Why the trailing message breakpoint is deliberately excluded
    ///
    /// The default/explicit caching policy also marks the *last message* of the
    /// conversation so the full conversation prefix is cacheable. That breakpoint
    /// segment changes **every single turn by design** — the conversation grows, so
    /// the last message (and the block carrying the marker) is different on every
    /// request. Folding it into this hash made the drift guard fire on every turn for
    /// every Anthropic-format model, turning a useful "your stable prefix drifted"
    /// alarm into wall-to-wall noise that masked real drift. Excluding it is correct:
    /// the trailing breakpoint extends the cache to the conversation tail, but the
    /// *earlier* tool/system breakpoints still hit, and those are exactly what this
    /// guard watches for unexpected drift.
    ///
    /// It is intentionally allocation-light: it folds the relevant `serde_json`
    /// values into a `DefaultHasher` field-by-field rather than re-serializing or
    /// cloning the whole body. The value is only stable *within a single process
    /// run* (`DefaultHasher` is not portable), which is all a within-run drift check
    /// needs. Returns `None` when no stable (tool/system) `cache_control` marker is
    /// present — caching is either inactive or carried solely by the trailing message
    /// breakpoint, and in both cases there is no stable prefix worth guarding.
    pub(super) fn stable_prefix_hash(body: &Value) -> Option<u64> {
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

        // NOTE: the trailing *message* breakpoint is intentionally NOT folded here.
        // It marks the last message so the conversation tail is cacheable, but that
        // block changes every turn by design (the conversation grows). Including it
        // made the drift guard fire on every turn. See the doc comment above.

        if marked == 0 {
            return None;
        }
        marked.hash(&mut hasher);
        Some(hasher.finish())
    }

    pub(crate) fn build_request(
        &self,
        conversation: &Conversation,
        tools: &[Value],
        tool_choice: Option<ToolChoice>,
    ) -> Value {
        let mut messages = Self::serialize_messages(conversation);
        let mut system_blocks = Self::system_blocks(conversation);

        // Caching policy. Explicit: the chat layer annotates system messages
        // with `anthropic_cache_breakpoint` metadata describing the stable
        // prefix / dynamic tail split (ADR-043 §8). Default: conversations
        // without that metadata (worker/task sessions assemble their system
        // prompt as one stable string) still deserve caching — mark the whole
        // tool array and system prompt plus the trailing message breakpoint.
        // Gated on non-empty tools so one-shot utility calls (compaction
        // summaries) don't pay a cache write they will never read back.
        let explicit_cache = Self::has_cache_metadata(conversation);
        let default_cache = !explicit_cache && !tools.is_empty();

        if default_cache && let Some(last) = system_blocks.last_mut() {
            last.cache_control = Some(json!({"type": "ephemeral"}));
        }

        // Message-level cache breakpoint: mark the last message so the full
        // conversation prefix is cacheable across consecutive turns.
        if explicit_cache || default_cache {
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
        // design/aiu0-roadmap: until live MiniMax captures prove inline
        // `<think>` leakage, this wave intentionally relies on Anthropic's
        // structured `thinking` channel and existing parser rather than adding
        // a fallback extractor.
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

        let tool_cache_control = Self::tool_definition_cache_control(conversation)
            .or_else(|| default_cache.then(|| json!({"type": "ephemeral"})));
        if let Some(serialized_tools) = Self::serialize_tools_for_request(
            tools,
            tool_cache_control.as_ref(),
            self.config.tool_schema_compat,
        ) {
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
    /// instance's previous request and `warn!` on an unexpected change.
    ///
    /// "Stable prefix" here is the model id + cached tool definitions + cached system
    /// blocks — the segments that are supposed to be identical turn-to-turn. The
    /// trailing message breakpoint is excluded on purpose: it grows with the
    /// conversation every turn, so hashing it would make this guard warn constantly
    /// and mask genuine drift. See [`Self::stable_prefix_hash`] for the full rationale.
    fn check_prefix_drift(&self, body: &Value) {
        let Some(current) = Self::stable_prefix_hash(body) else {
            return; // no stable (tool/system) cache marker => nothing to guard
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

    pub(crate) fn effective_url(&self) -> String {
        // Anthropic-compatible vendors (MiniMax / GLM coding plans) publish
        // base URLs that already end in `/v1`; don't double the segment.
        let base = self.config.base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        format!("{base}/v1/messages")
    }

    pub(crate) fn extra_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();

        for (name, value) in &self.config.provider_headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }

        // Anthropic version header (always required)
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );

        headers
    }
}
