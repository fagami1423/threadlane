//! Native Anthropic Messages API provider (`anthropic/<model>`).
//!
//! The router still builds a Chat Completions payload; this module translates
//! it (purely, see [`chat_payload_to_messages_request`]) into the Messages
//! wire format and maps the SSE stream back onto the shared `StreamEvent`
//! contract. Credentials are API keys only.

use crate::openai::{ProviderUsage, StreamEvent, ToolCall, ToolCallFunction};
use crate::router::{PayloadFormat, PayloadSource};
use crate::traits::ModelProvider;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const ANTHROPIC_MODEL_PREFIX: &str = "anthropic/";
/// `max_tokens` is required by the Messages API. Callers that set their own
/// limit on the payload win. Current models think adaptively and thinking
/// tokens count against this cap, so the default leaves generous room.
const DEFAULT_MAX_TOKENS: u64 = 32_000;

/// `ANTHROPIC_API_KEY` from the environment, if set and non-blank.
pub fn api_key_from_env() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

pub fn strip_anthropic_prefix(model: &str) -> &str {
    model.strip_prefix(ANTHROPIC_MODEL_PREFIX).unwrap_or(model)
}

#[derive(Debug, Clone, Default)]
pub struct AnthropicClient {
    client: reqwest::Client,
    api_key: Option<String>,
}

impl AnthropicClient {
    pub(crate) fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: None,
        }
    }

    /// Injects an explicit API key (PR 2 wires stored keys); otherwise
    /// requests use `ANTHROPIC_API_KEY` from the environment.
    #[cfg(test)]
    pub(crate) fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        self.api_key = (!key.trim().is_empty()).then_some(key);
        self
    }

    fn api_key(&self) -> Option<String> {
        self.api_key
            .clone()
            .filter(|key| !key.trim().is_empty())
            .or_else(api_key_from_env)
    }

    fn messages_url() -> String {
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .map(|base| base.trim().to_string())
            .filter(|base| !base.is_empty())
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string());
        format!("{}/v1/messages", base.trim_end_matches('/'))
    }
}

// ---------------------------------------------------------------------------
// Request conversion (Chat Completions payload -> Messages request)
// ---------------------------------------------------------------------------

/// Anthropic tool ids must match `^[a-zA-Z0-9_-]+$`. Applied identically to
/// `tool_use.id` and `tool_result.tool_use_id` so pairs stay linked.
fn sanitize_tool_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "call_0".to_string()
    } else {
        cleaned
    }
}

fn text_block(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"type": "text", "text": text}))
}

/// `data:<media>;base64,<data>` -> image block. Remote URLs are not fetched.
fn image_block(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(json!({
        "type": "image",
        "source": {"type": "base64", "media_type": media_type, "data": data}
    }))
}

/// Converts string-or-parts message content into Messages content blocks.
fn content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => text_block(text).into_iter().collect(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => text_block(part.get("text").and_then(Value::as_str)?),
                Some("image_url") => {
                    let url = part
                        .get("image_url")
                        .and_then(|image| image.get("url").or(Some(image)))
                        .and_then(Value::as_str)?;
                    image_block(url)
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_tool_arguments(arguments: &Value, name: &str) -> Value {
    let parsed = match arguments {
        Value::String(raw) if raw.trim().is_empty() => None,
        Value::String(raw) => serde_json::from_str::<Value>(raw).ok(),
        Value::Object(_) => Some(arguments.clone()),
        _ => None,
    };
    match parsed {
        Some(value) if value.is_object() => value,
        _ => {
            if !matches!(arguments, Value::String(raw) if raw.trim().is_empty()) {
                tracing::warn!(tool = name, "invalid tool arguments; sending empty input");
            }
            json!({})
        }
    }
}

/// Appends `blocks` as a `role` message, merging into the previous message
/// when it has the same role (the API requires strict alternation).
fn push_message(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": blocks}));
}

/// `tool_result` blocks must precede any other content in a user message.
fn order_tool_results_first(messages: &mut [Value]) {
    for message in messages {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            blocks.sort_by_key(|block| {
                u8::from(block.get("type").and_then(Value::as_str) != Some("tool_result"))
            });
        }
    }
}

/// Pure translation of the router's Chat Completions payload into a streaming
/// Messages API request body.
pub fn chat_payload_to_messages_request(payload: &Value) -> Value {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(strip_anthropic_prefix)
        .unwrap_or_default();

    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for message in payload
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let content = message.get("content").unwrap_or(&Value::Null);
        match message.get("role").and_then(Value::as_str) {
            Some("system") | Some("developer") => {
                let text: Vec<String> = content_blocks(content)
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect();
                if !text.is_empty() {
                    system.push(text.join("\n"));
                }
            }
            Some("user") => push_message(&mut messages, "user", content_blocks(content)),
            Some("assistant") => {
                let mut blocks = content_blocks(content);
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    let function = call.get("function").unwrap_or(&Value::Null);
                    let name = function.get("name").and_then(Value::as_str).unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": sanitize_tool_id(id),
                        "name": name,
                        "input": parse_tool_arguments(
                            function.get("arguments").unwrap_or(&Value::Null),
                            name,
                        ),
                    }));
                }
                push_message(&mut messages, "assistant", blocks);
            }
            Some("tool") => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut result = Map::new();
                result.insert("type".into(), "tool_result".into());
                result.insert("tool_use_id".into(), sanitize_tool_id(id).into());
                let blocks = content_blocks(content);
                // A tool_result always needs content so the model sees that the
                // call completed, even when the tool printed nothing.
                let body = if blocks.is_empty() {
                    Value::String("(no output)".to_string())
                } else if blocks
                    .iter()
                    .all(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                {
                    Value::String(
                        blocks
                            .iter()
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                } else {
                    Value::Array(blocks)
                };
                result.insert("content".into(), body);
                push_message(&mut messages, "user", vec![Value::Object(result)]);
            }
            _ => {}
        }
    }
    order_tool_results_first(&mut messages);

    let max_tokens = payload
        .get("max_tokens")
        .or_else(|| payload.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let mut request = Map::new();
    request.insert("model".into(), model.into());
    request.insert("max_tokens".into(), max_tokens.into());
    request.insert("stream".into(), true.into());
    if !system.is_empty() {
        request.insert("system".into(), system.join("\n\n").into());
    }
    request.insert("messages".into(), Value::Array(messages));

    let tools: Vec<Value> = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?;
            let mut out = Map::new();
            out.insert("name".into(), name.into());
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                out.insert("description".into(), description.into());
            }
            out.insert(
                "input_schema".into(),
                function
                    .get("parameters")
                    .filter(|schema| schema.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            );
            Some(Value::Object(out))
        })
        .collect();
    if !tools.is_empty() {
        request.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(request)
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental parser for the Messages SSE stream. Feed raw text with
/// [`SseParser::push`]; each call returns the events ready to emit. Terminal
/// state is reached on `message_stop` or an `error` event.
#[derive(Default)]
pub struct SseParser {
    buffer: String,
    /// Content block index -> in-flight tool call.
    tool_blocks: std::collections::BTreeMap<u64, PendingToolCall>,
    finished_calls: Vec<(u64, PendingToolCall)>,
    usage: ProviderUsage,
    done: bool,
}

impl SseParser {
    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn push(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some(line_end) = self.buffer.find('\n') {
            let line = self.buffer[..line_end].trim().to_string();
            self.buffer.drain(..=line_end);
            if self.done {
                continue;
            }
            // `event:` lines are redundant: every data payload carries `type`.
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            self.handle_event(&value, &mut events);
        }
        events
    }

    fn handle_event(&mut self, event: &Value, out: &mut Vec<StreamEvent>) {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = event.get("message").and_then(|m| m.get("usage")) {
                    self.read_usage(usage);
                }
            }
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    self.tool_blocks.insert(
                        index,
                        PendingToolCall {
                            id: block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            name: name.to_string(),
                            arguments: String::new(),
                        },
                    );
                    out.push(StreamEvent::ToolCallStart {
                        name: name.to_string(),
                    });
                }
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                out.push(StreamEvent::ContentToken(text.to_string()));
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(chunk) = delta.get("partial_json").and_then(Value::as_str) {
                            if let Some(call) = self.tool_blocks.get_mut(&index) {
                                call.arguments.push_str(chunk);
                                if !chunk.is_empty() {
                                    out.push(StreamEvent::ToolCallArgsDelta {
                                        args_chunk: chunk.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    // thinking / signature deltas arrive with extended thinking (PR 3).
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                if let Some(call) = self.tool_blocks.remove(&index) {
                    self.finished_calls.push((index, call));
                }
            }
            Some("message_delta") => {
                if let Some(usage) = event.get("usage") {
                    self.read_usage(usage);
                }
            }
            Some("message_stop") => {
                self.done = true;
                // Close any block the server never stopped explicitly.
                let open = std::mem::take(&mut self.tool_blocks);
                self.finished_calls.extend(open);
                let mut calls = std::mem::take(&mut self.finished_calls);
                calls.sort_by_key(|(index, _)| *index);
                let tool_calls = calls
                    .into_iter()
                    .filter(|(_, call)| !call.name.is_empty())
                    .map(|(_, call)| ToolCall {
                        id: call.id,
                        r#type: "function".to_string(),
                        function: ToolCallFunction {
                            name: call.name,
                            arguments: if call.arguments.trim().is_empty() {
                                "{}".to_string()
                            } else {
                                call.arguments
                            },
                        },
                        thought_signature: None,
                    })
                    .collect();
                let mut usage = self.usage;
                usage.total_tokens = usage
                    .input_tokens
                    .saturating_add(usage.cache_read_tokens)
                    .saturating_add(usage.cache_write_tokens)
                    .saturating_add(usage.output_tokens);
                out.push(StreamEvent::Finished { tool_calls, usage });
            }
            Some("error") => {
                self.done = true;
                let error = event.get("error").unwrap_or(&Value::Null);
                out.push(StreamEvent::Error(describe_api_error(
                    error.get("type").and_then(Value::as_str),
                    error.get("message").and_then(Value::as_str),
                )));
            }
            // `ping` and unknown future events are ignored.
            _ => {}
        }
    }

    /// Anthropic reports `input_tokens` excluding cache tokens, which matches
    /// the normalized `ProviderUsage` convention (uncached input) directly.
    /// `message_delta` usage is cumulative, so present fields overwrite.
    fn read_usage(&mut self, usage: &Value) {
        let field = |name: &str| usage.get(name).and_then(Value::as_u64);
        let clamp = |value: u64| value.min(u32::MAX as u64) as u32;
        if let Some(v) = field("input_tokens") {
            self.usage.input_tokens = clamp(v);
        }
        if let Some(v) = field("output_tokens") {
            self.usage.output_tokens = clamp(v);
        }
        if let Some(v) = field("cache_read_input_tokens") {
            self.usage.cache_read_tokens = clamp(v);
        }
        if let Some(v) = field("cache_creation_input_tokens") {
            self.usage.cache_write_tokens = clamp(v);
        }
    }
}

fn describe_api_error(kind: Option<&str>, message: Option<&str>) -> String {
    let detail = message.unwrap_or("no details");
    match kind {
        Some("authentication_error") => "Invalid Anthropic API key".to_string(),
        Some("rate_limit_error") => {
            format!("Anthropic rate limited, try again shortly ({detail})")
        }
        Some("overloaded_error") => {
            format!("Anthropic is overloaded, try again shortly ({detail})")
        }
        Some(kind) => format!("Anthropic API error ({kind}): {detail}"),
        None => format!("Anthropic API error: {detail}"),
    }
}

/// Maps a non-success HTTP response to a readable message. The key is never
/// echoed: only the status and the API's own error body are included.
pub fn describe_http_error(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    let kind = error.and_then(|e| e.get("type")).and_then(Value::as_str);
    let message = error.and_then(|e| e.get("message")).and_then(Value::as_str);
    match status {
        401 => "Invalid Anthropic API key".to_string(),
        429 | 529 => format!(
            "Anthropic rate limited / overloaded ({status}), try again shortly{}",
            message.map(|m| format!(": {m}")).unwrap_or_default()
        ),
        _ if kind.is_some() || message.is_some() => {
            format!(
                "Anthropic API error ({status}): {}",
                describe_api_error(kind, message)
            )
        }
        _ => format!("Anthropic API error ({status})"),
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

#[async_trait]
impl ModelProvider for AnthropicClient {
    fn provider_id(&self) -> &'static str {
        "anthropic"
    }

    fn supports_model(&self, model: &str) -> bool {
        model.starts_with(ANTHROPIC_MODEL_PREFIX)
    }

    async fn stream_chat_completion(
        &self,
        payload_source: PayloadSource,
        _prompt_cache_key: Option<String>,
        event_tx: mpsc::Sender<StreamEvent>,
    ) {
        let Some(api_key) = self.api_key() else {
            let _ = event_tx
                .send(StreamEvent::Error(
                    "No Anthropic API key provided. Set the ANTHROPIC_API_KEY environment variable."
                        .to_string(),
                ))
                .await;
            return;
        };

        let chat_payload = payload_source.resolve(PayloadFormat::ChatCompletions).await;
        let body = chat_payload_to_messages_request(&chat_payload);

        // Retry transient failures only before a stream has started; replaying a
        // partially consumed stream could duplicate content or tool calls.
        let mut retries = 0;
        let response = loop {
            let response = match self
                .client
                .post(Self::messages_url())
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "text/event-stream")
                .header(USER_AGENT, "threadlane/1.0")
                .json(&body)
                .send()
                .await
            {
                Ok(res) => res,
                Err(err) => {
                    // reqwest errors can embed the URL but never request headers.
                    let _ = event_tx
                        .send(StreamEvent::Error(format!(
                            "Anthropic API request failed: {err}"
                        )))
                        .await;
                    return;
                }
            };
            if retries >= 2 || !matches!(response.status().as_u16(), 500 | 502 | 503 | 504) {
                break response;
            }
            tracing::warn!(
                status = %response.status(),
                retry = retries + 1,
                "retrying transient Anthropic server failure"
            );
            drop(response);
            tokio::select! {
                _ = event_tx.closed() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(1 << retries)) => {}
            }
            retries += 1;
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let _ = event_tx
                .send(StreamEvent::Error(describe_http_error(status, &text)))
                .await;
            return;
        }

        let mut parser = SseParser::default();
        let mut stream = response.bytes_stream();
        // Chunks can split a multi-byte character; decode only complete prefixes.
        let mut pending_bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    let _ = event_tx
                        .send(StreamEvent::Error(format!(
                            "Error reading Anthropic stream: {err}"
                        )))
                        .await;
                    return;
                }
            };
            pending_bytes.extend_from_slice(&chunk);
            let valid_up_to = match std::str::from_utf8(&pending_bytes) {
                Ok(_) => pending_bytes.len(),
                Err(err) => err.valid_up_to(),
            };
            let text = String::from_utf8_lossy(&pending_bytes[..valid_up_to]).into_owned();
            pending_bytes.drain(..valid_up_to);
            for event in parser.push(&text) {
                if event_tx.send(event).await.is_err() {
                    return;
                }
            }
            if parser.is_done() {
                return;
            }
        }
        if !parser.is_done() {
            let _ = event_tx
                .send(StreamEvent::Error(
                    "Anthropic stream ended before the message completed".to_string(),
                ))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(parser: &mut SseParser, raw: &str) -> Vec<StreamEvent> {
        parser.push(raw)
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| {
                format!(
                    "event: {}\ndata: {}\n\n",
                    e["type"].as_str().unwrap_or("x"),
                    e
                )
            })
            .collect()
    }

    // ---- conversion ----

    #[test]
    fn extracts_system_and_strips_prefix() {
        let request = chat_payload_to_messages_request(&json!({
            "model": "anthropic/claude-sonnet-x",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "system", "content": "be kind"},
                {"role": "user", "content": "hi"},
            ],
        }));
        assert_eq!(request["model"], "claude-sonnet-x");
        assert_eq!(request["system"], "be brief\n\nbe kind");
        assert_eq!(request["stream"], true);
        assert_eq!(request["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(
            request["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
        );
        assert!(request.get("tools").is_none());
    }

    #[test]
    fn honors_payload_max_tokens() {
        let request = chat_payload_to_messages_request(&json!({
            "model": "anthropic/m", "max_tokens": 256, "messages": []
        }));
        assert_eq!(request["max_tokens"], 256);
    }

    #[test]
    fn merges_consecutive_same_role_and_drops_empty_text() {
        let request = chat_payload_to_messages_request(&json!({
            "model": "anthropic/m",
            "messages": [
                {"role": "user", "content": "a"},
                {"role": "user", "content": "   "},
                {"role": "user", "content": "b"},
                {"role": "assistant", "content": ""},
                {"role": "assistant", "content": "c"},
            ],
        }));
        assert_eq!(
            request["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "c"}]},
            ])
        );
    }

    #[test]
    fn tool_call_round_trip_with_merged_results() {
        let request = chat_payload_to_messages_request(&json!({
            "model": "anthropic/m",
            "messages": [
                {"role": "user", "content": "list files"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}},
                    {"id": "call.2", "type": "function",
                     "function": {"name": "read_file", "arguments": ""}},
                ]},
                {"role": "tool", "tool_call_id": "call_1", "name": "list_dir", "content": "a.rs"},
                {"role": "tool", "tool_call_id": "call.2", "name": "read_file", "content": ""},
                {"role": "user", "content": "thanks"},
            ],
        }));
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1]["content"],
            json!([
                {"type": "tool_use", "id": "call_1", "name": "list_dir", "input": {"path": "."}},
                {"type": "tool_use", "id": "call_2", "name": "read_file", "input": {}},
            ])
        );
        // Both results plus the follow-up text share one user message, results first.
        assert_eq!(
            messages[2]["content"],
            json!([
                {"type": "tool_result", "tool_use_id": "call_1", "content": "a.rs"},
                {"type": "tool_result", "tool_use_id": "call_2", "content": "(no output)"},
                {"type": "text", "text": "thanks"},
            ])
        );
    }

    #[test]
    fn invalid_tool_arguments_become_empty_input() {
        let request = chat_payload_to_messages_request(&json!({
            "model": "anthropic/m",
            "messages": [{"role": "assistant", "tool_calls": [
                {"id": "c", "function": {"name": "t", "arguments": "{not json"}}]}],
        }));
        assert_eq!(request["messages"][0]["content"][0]["input"], json!({}));
    }

    #[test]
    fn maps_tool_schemas() {
        let request = chat_payload_to_messages_request(&json!({
            "model": "anthropic/m",
            "messages": [],
            "tools": [
                {"type": "function", "function": {
                    "name": "grep", "description": "search",
                    "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}},
                {"type": "function", "function": {"name": "noop"}},
            ],
        }));
        assert_eq!(
            request["tools"],
            json!([
                {"name": "grep", "description": "search",
                 "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}},
                {"name": "noop", "input_schema": {"type": "object", "properties": {}}},
            ])
        );
    }

    #[test]
    fn converts_data_url_images_and_skips_remote_urls() {
        let request = chat_payload_to_messages_request(&json!({
            "model": "anthropic/m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "auto"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
            ]}],
        }));
        assert_eq!(
            request["messages"][0]["content"],
            json!([
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ])
        );
    }

    // ---- SSE ----

    #[test]
    fn parses_text_stream_with_usage() {
        let raw = sse(&[
            json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 10, "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 2, "output_tokens": 1}}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Hel"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 7}}),
            json!({"type": "message_stop"}),
        ]);
        let mut parser = SseParser::default();
        let events = collect(&mut parser, &raw);
        assert!(parser.is_done());
        assert!(matches!(&events[0], StreamEvent::ContentToken(t) if t == "Hel"));
        assert!(matches!(&events[1], StreamEvent::ContentToken(t) if t == "lo"));
        match &events[2] {
            StreamEvent::Finished { tool_calls, usage } => {
                assert!(tool_calls.is_empty());
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.cache_read_tokens, 5);
                assert_eq!(usage.cache_write_tokens, 2);
                assert_eq!(usage.total_tokens, 24);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn parses_tool_call_with_split_json_across_chunks() {
        let raw = sse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 3}}}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "toolu_1", "name": "list_dir", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "th\":\".\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                   "usage": {"output_tokens": 4}}),
            json!({"type": "message_stop"}),
        ]);
        // Feed in awkward slices that split lines mid-way.
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for slice in raw.as_bytes().chunks(17) {
            events.extend(parser.push(std::str::from_utf8(slice).unwrap()));
        }
        assert!(matches!(&events[0], StreamEvent::ContentToken(t) if t == "ok"));
        assert!(matches!(&events[1], StreamEvent::ToolCallStart { name } if name == "list_dir"));
        assert!(
            matches!(&events[2], StreamEvent::ToolCallArgsDelta { args_chunk } if args_chunk == "{\"pa")
        );
        match events.last().unwrap() {
            StreamEvent::Finished { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "toolu_1");
                assert_eq!(tool_calls[0].r#type, "function");
                assert_eq!(tool_calls[0].function.name, "list_dir");
                assert_eq!(tool_calls[0].function.arguments, "{\"path\":\".\"}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_two_tool_calls_in_block_order() {
        let raw = sse(&[
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "tool_use", "id": "a", "name": "one", "input": {}}}),
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "b", "name": "two", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"x\":1}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_stop"}),
        ]);
        let mut parser = SseParser::default();
        let events = collect(&mut parser, &raw);
        match events.last().unwrap() {
            StreamEvent::Finished { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 2);
                assert_eq!(tool_calls[0].function.name, "one");
                // No arguments streamed -> valid empty object.
                assert_eq!(tool_calls[0].function.arguments, "{}");
                assert_eq!(tool_calls[1].function.name, "two");
                assert_eq!(tool_calls[1].function.arguments, "{\"x\":1}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn error_event_is_terminal_and_readable() {
        let raw = sse(&[
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "partial"}}),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            json!({"type": "message_stop"}),
        ]);
        let mut parser = SseParser::default();
        let events = collect(&mut parser, &raw);
        assert!(parser.is_done());
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1], StreamEvent::Error(m) if m.contains("overloaded")));
    }

    #[test]
    fn http_errors_are_mapped_without_leaking_keys() {
        assert_eq!(describe_http_error(401, "{}"), "Invalid Anthropic API key");
        assert!(describe_http_error(429, "").contains("rate limited"));
        assert!(describe_http_error(529, "").contains("overloaded"));
        let msg = describe_http_error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
        );
        assert!(msg.contains("400") && msg.contains("bad"));
    }

    // ---- client ----

    #[test]
    fn client_supports_only_prefixed_models() {
        let client = AnthropicClient::new();
        assert!(client.supports_model("anthropic/claude-sonnet-x"));
        assert!(!client.supports_model("claude-sonnet-x"));
        assert!(!client.supports_model("opencode-go/kimi-k3"));
        assert_eq!(client.provider_id(), "anthropic");
    }

    #[test]
    fn explicit_key_wins_and_blank_is_ignored() {
        let client = AnthropicClient::new().with_api_key("sk-test");
        assert_eq!(client.api_key().as_deref(), Some("sk-test"));
        assert!(AnthropicClient::new().with_api_key("  ").api_key.is_none());
    }

    // ---- local mock server ----

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serializes tests that point `ANTHROPIC_BASE_URL` at a mock server.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serves one canned HTTP response and returns the raw request it saw.
    async fn serve_once(
        status_line: &'static str,
        content_type: &'static str,
        body_parts: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request).into_owned();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            for part in body_parts {
                socket.write_all(part.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            socket.shutdown().await.ok();
            String::from_utf8_lossy(&request).into_owned()
        });
        (base, handle)
    }

    async fn run_client(base: &str, payload: Value) -> Vec<StreamEvent> {
        std::env::set_var("ANTHROPIC_BASE_URL", base);
        let client = AnthropicClient::new().with_api_key("sk-ant-test-key");
        let (tx, mut rx) = mpsc::channel(64);
        client
            .stream_chat_completion(PayloadSource::ChatCompletions(payload), None, tx)
            .await;
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn streams_a_tool_turn_from_a_local_mock_server() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let body = sse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 9}}}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "tool_use", "id": "toolu_9", "name": "list_dir", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\".\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ]);
        // Split mid-stream to exercise buffering over a real socket.
        let mid = body.len() / 2;
        let (base, server) = serve_once(
            "200 OK",
            "text/event-stream",
            vec![body[..mid].to_string(), body[mid..].to_string()],
        )
        .await;
        let events = run_client(
            &base,
            json!({
                "model": "anthropic/claude-test",
                "messages": [{"role": "user", "content": "list files"}],
                "tools": [{"type": "function", "function": {"name": "list_dir", "parameters": {"type": "object"}}}],
            }),
        )
        .await;
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/messages "), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("x-api-key: sk-ant-test-key"));
        assert!(lower.contains("anthropic-version: 2023-06-01"));
        assert!(request.contains("\"model\":\"claude-test\""));
        assert!(request.contains("\"input_schema\""));
        assert!(matches!(&events[0], StreamEvent::ToolCallStart { name } if name == "list_dir"));
        match events.last().unwrap() {
            StreamEvent::Finished { tool_calls, usage } => {
                assert_eq!(tool_calls[0].id, "toolu_9");
                assert_eq!(tool_calls[0].function.arguments, "{\"path\":\".\"}");
                assert_eq!(usage.input_tokens, 9);
                assert_eq!(usage.output_tokens, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn maps_http_401_without_leaking_the_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (base, server) = serve_once(
            "401 Unauthorized",
            "application/json",
            vec![r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.to_string()],
        )
        .await;
        let events = run_client(
            &base,
            json!({"model": "anthropic/claude-test", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        server.await.unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error(message) => {
                assert_eq!(message, "Invalid Anthropic API key");
                assert!(!message.contains("sk-ant-test-key"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn truncated_stream_reports_an_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let body = sse(&[json!({"type": "content_block_delta", "index": 0,
                                "delta": {"type": "text_delta", "text": "hi"}})]);
        let (base, server) = serve_once("200 OK", "text/event-stream", vec![body]).await;
        let events = run_client(
            &base,
            json!({"model": "anthropic/claude-test", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        server.await.unwrap();
        assert!(matches!(&events[0], StreamEvent::ContentToken(t) if t == "hi"));
        assert!(
            matches!(events.last().unwrap(), StreamEvent::Error(m) if m.contains("ended before"))
        );
    }
}
