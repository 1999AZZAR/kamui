use super::{
    ChatRequest, ChatResponse, Message, Provider, StreamEvent, ToolCall, ToolDefinition, Usage,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    base_url: String,
    /// When set, chat POSTs here instead of `{base_url}/chat/completions`.
    ///
    /// Absolute HTTP(S) URLs are used as-is. Paths starting with `/` replace the
    /// path of `base_url` (so `base_url = https://api.orvix.id/v1` +
    /// `completions_path = /coding/completions` → Orvix Coding Plan). Relative
    /// paths append under `base_url`.
    completions_path: Option<String>,
    /// When true, every chat request must carry `ChatRequest::session_id` and it
    /// is sent as a top-level `session_id` field (Orvix Coding Plan).
    send_session_id: bool,
}

impl OpenAIProvider {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self::with_options(api_key, base_url, None, false)
    }

    pub fn with_options(
        api_key: String,
        base_url: String,
        completions_path: Option<String>,
        send_session_id: bool,
    ) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            completions_path,
            send_session_id,
        }
    }

    pub fn from_profile(profile: &crate::config::Profile) -> Self {
        Self::with_options(
            profile.api_key.clone(),
            profile.base_url.clone(),
            profile.completions_path.clone(),
            profile.send_session_id,
        )
    }

    fn chat_url(&self) -> String {
        match &self.completions_path {
            Some(path) if path.starts_with("http://") || path.starts_with("https://") => {
                path.trim_end_matches('/').to_string()
            }
            Some(path) if path.starts_with('/') => match origin_of(&self.base_url) {
                Some(origin) => format!("{origin}{path}"),
                None => format!("{}{path}", self.base_url),
            },
            Some(path) => format!("{}/{}", self.base_url, path.trim_start_matches('/')),
            None => format!("{}/chat/completions", self.base_url),
        }
    }

    fn resolve_session_id<'a>(&self, request: &'a ChatRequest) -> Result<Option<&'a str>> {
        if !self.send_session_id {
            return Ok(None);
        }
        match request.session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => Ok(Some(id)),
            None => bail!(
                "this provider requires session_id (Orvix Coding Plan); no active Kamui session id was provided"
            ),
        }
    }

    /// Discover model identifiers exposed by an OpenAI-compatible provider.
    pub async fn list_models(api_key: &str, base_url: &str) -> Result<Vec<String>> {
        let response = Client::new()
            .get(format!("{}/models", base_url.trim_end_matches('/')))
            .bearer_auth(api_key)
            .send()
            .await
            .context("failed to call the provider models endpoint")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }

        let response: ModelsResponse = response
            .json()
            .await
            .context("provider returned an invalid models response")?;
        let mut models = response
            .data
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        if models.is_empty() {
            bail!("provider returned no models");
        }
        Ok(models)
    }
}

/// Scheme + host (+ port) of an HTTP(S) base URL, with no path.
fn origin_of(base: &str) -> Option<String> {
    let trimmed = base.trim_end_matches('/');
    let scheme_sep = trimmed.find("://")?;
    let after_scheme = &trimmed[scheme_sep + 3..];
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    Some(format!("{}{}", &trimmed[..scheme_sep + 3], &after_scheme[..host_end]))
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
}

// Request wire types. These belong to the provider; the core stays agnostic and never
// serializes its own message types into an OpenAI-shaped payload.

#[derive(Serialize)]
struct OpenAIRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Serialize)]
struct OpenAIStreamRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    stream: bool,
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<WireContent<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// Message content is a plain string unless images are attached, in which case OpenAI expects an
/// array of typed parts.
#[derive(Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
    Text(&'a str),
    Parts(Vec<WirePart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum WirePart<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireImageUrl },
}

#[derive(Serialize)]
struct WireImageUrl {
    url: String,
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall<'a>,
}

#[derive(Serialize)]
struct WireFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction<'a>,
}

#[derive(Serialize)]
struct WireToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

fn wire_messages(messages: &[Message]) -> Vec<WireMessage<'_>> {
    messages.iter().map(wire_message).collect()
}

fn wire_message(message: &Message) -> WireMessage<'_> {
    let content = if !message.images.is_empty() {
        // With images, content becomes an array of text and image parts.
        let mut parts = Vec::with_capacity(message.images.len() + 1);
        if !message.content.is_empty() {
            parts.push(WirePart::Text {
                text: &message.content,
            });
        }
        for image in &message.images {
            parts.push(WirePart::ImageUrl {
                image_url: WireImageUrl {
                    url: format!("data:{};base64,{}", image.media_type, image.data),
                },
            });
        }
        Some(WireContent::Parts(parts))
    } else if message.content.is_empty() && !message.tool_calls.is_empty() {
        // OpenAI expects a null content on an assistant turn that only requests tool calls.
        None
    } else {
        Some(WireContent::Text(&message.content))
    };
    WireMessage {
        role: message.role_name(),
        content,
        tool_calls: message
            .tool_calls
            .iter()
            .map(|call| WireToolCall {
                id: &call.id,
                kind: "function",
                function: WireFunctionCall {
                    name: &call.name,
                    arguments: &call.arguments,
                },
            })
            .collect(),
        tool_call_id: message.tool_call_id.as_deref(),
    }
}

fn wire_tools(tools: &[ToolDefinition]) -> Vec<WireTool<'_>> {
    tools
        .iter()
        .map(|tool| WireTool {
            kind: "function",
            function: WireToolFunction {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
            },
        })
        .collect()
}

// Response wire types.

// Embeddings request/response wire types, for `/index`/`search_code`.

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

/// Sort embeddings back into input order (the API is expected to already return them in order,
/// but the wire contract only guarantees `index`, not array position) and check the count matches
/// so a caller zipping the result against its input never silently misaligns.
fn embeddings_into_vectors(
    mut response: EmbeddingsResponse,
    expected: usize,
) -> Result<Vec<Vec<f32>>> {
    response.data.sort_by_key(|item| item.index);
    if response.data.len() != expected {
        bail!(
            "provider returned {} embedding(s) for {expected} input(s)",
            response.data.len()
        );
    }
    Ok(response
        .data
        .into_iter()
        .map(|item| item.embedding)
        .collect())
}

/// Some OpenAI-compatible providers send JSON `null` for empty arrays. `#[serde(default)]`
/// alone only covers a missing field, not an explicit null.
fn null_as_empty_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Debug, Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunctionCall,
}

#[derive(Debug, Deserialize)]
struct ResponseFunctionCall {
    name: String,
    arguments: String,
}

fn response_into_chat(mut response: OpenAIResponse) -> Result<ChatResponse> {
    let choice = response
        .choices
        .pop()
        .context("provider returned no choices")?;
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|call| ToolCall {
            id: call.id,
            name: call.function.name,
            arguments: call.function.arguments,
        })
        .collect();
    Ok(ChatResponse {
        content: choice.message.content.unwrap_or_default(),
        tool_calls,
        usage: response.usage,
        finish_reason: choice.finish_reason,
    })
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamChunk {
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    tool_calls: Vec<StreamToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

/// Accumulates the pieces of a streamed response until the terminating event. Tool calls arrive
/// as index-keyed fragments across many deltas, so they are reassembled here.
#[derive(Default)]
struct StreamState {
    usage: Usage,
    finish_reason: String,
    tool_calls: Vec<PartialToolCall>,
}

#[derive(Clone, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn assemble_tool_calls(partials: Vec<PartialToolCall>) -> Vec<ToolCall> {
    partials
        .into_iter()
        .filter(|partial| !partial.id.is_empty() && !partial.name.is_empty())
        .map(|partial| ToolCall {
            id: partial.id,
            name: partial.name,
            arguments: partial.arguments,
        })
        .collect()
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let session_id = self.resolve_session_id(&request)?;
        let body = OpenAIRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
            session_id,
        };
        let response = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("failed to call provider")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }

        let response: OpenAIResponse = response
            .json()
            .await
            .context("provider returned an invalid response")?;
        response_into_chat(response)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::UnboundedReceiver<Result<StreamEvent>>> {
        let session_id = self.resolve_session_id(&request)?;
        let body = OpenAIStreamRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            session_id,
        };
        let mut response = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("failed to call provider")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let result = read_stream(&mut response, &sender).await;
            if let Err(error) = result {
                let _ = sender.send(Err(error));
            }
        });
        Ok(receiver)
    }

    async fn embed(&self, model: &str, input: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let body = EmbeddingsRequest {
            model,
            input: &input,
        };
        let response = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("failed to call provider")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }

        let response: EmbeddingsResponse = response
            .json()
            .await
            .context("provider returned an invalid embeddings response")?;
        embeddings_into_vectors(response, input.len())
    }
}

async fn read_stream(
    response: &mut reqwest::Response,
    sender: &mpsc::UnboundedSender<Result<StreamEvent>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    let mut state = StreamState::default();

    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read provider stream")?
    {
        buffer.extend_from_slice(&chunk);
        while let Some(end) = find_event_end(&buffer) {
            let event = buffer.drain(..end).collect::<Vec<_>>();
            let delimiter = if buffer.starts_with(b"\r\n\r\n") {
                4
            } else {
                2
            };
            buffer.drain(..delimiter);
            if parse_event(&event, sender, &mut state)? {
                let StreamState {
                    usage,
                    finish_reason,
                    tool_calls,
                } = std::mem::take(&mut state);
                sender
                    .send(Ok(StreamEvent::Done {
                        usage,
                        finish_reason,
                        tool_calls: assemble_tool_calls(tool_calls),
                    }))
                    .map_err(|_| anyhow::anyhow!("stream consumer disconnected"))?;
                return Ok(());
            }
        }
    }

    bail!("provider stream ended before [DONE]")
}

fn find_event_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| buffer.windows(2).position(|window| window == b"\n\n"))
}

fn parse_event(
    event: &[u8],
    sender: &mpsc::UnboundedSender<Result<StreamEvent>>,
    state: &mut StreamState,
) -> Result<bool> {
    let event = std::str::from_utf8(event).context("provider returned invalid UTF-8")?;
    for line in event.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            return Ok(true);
        }
        let chunk: OpenAIStreamChunk =
            serde_json::from_str(data).context("provider returned an invalid stream event")?;
        if let Some(chunk_usage) = chunk.usage {
            state.usage = chunk_usage;
        }
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                state.finish_reason = reason;
            }
            if let Some(content) = choice.delta.content.filter(|content| !content.is_empty()) {
                sender
                    .send(Ok(StreamEvent::Delta(content)))
                    .map_err(|_| anyhow::anyhow!("stream consumer disconnected"))?;
            }
            for delta in choice.delta.tool_calls {
                if delta.index >= state.tool_calls.len() {
                    state
                        .tool_calls
                        .resize(delta.index + 1, PartialToolCall::default());
                }
                let partial = &mut state.tool_calls[delta.index];
                if let Some(id) = delta.id {
                    partial.id = id;
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        partial.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        partial.arguments.push_str(&arguments);
                    }
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_finish_and_usage_events() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut state = StreamState::default();

        assert!(
            !parse_event(
                br#"data: {"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#,
                &sender,
                &mut state,
            )
            .unwrap()
        );
        assert!(!parse_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
            &sender,
            &mut state,
        )
        .unwrap());
        assert!(
            !parse_event(
                br#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                &sender,
                &mut state,
            )
            .unwrap()
        );
        assert!(parse_event(b"data: [DONE]", &sender, &mut state).unwrap());

        match receiver.try_recv().unwrap().unwrap() {
            StreamEvent::Delta(content) => assert_eq!(content, "Hello"),
            StreamEvent::Done { .. } => panic!("expected a delta"),
        }
        assert_eq!(state.usage.total_tokens, 5);
        assert_eq!(state.finish_reason, "stop");
    }

    #[test]
    fn assembles_tool_calls_streamed_across_deltas() {
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut state = StreamState::default();

        parse_event(
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":\""}}]},"finish_reason":null}]}"#,
            &sender,
            &mut state,
        )
        .unwrap();
        parse_event(
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"src/main.rs\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            &sender,
            &mut state,
        )
        .unwrap();

        let calls = assemble_tool_calls(state.tool_calls);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, r#"{"path":"src/main.rs"}"#);
    }

    #[test]
    fn drops_incomplete_tool_calls() {
        // A fragment that never received an id or name must not become a tool call.
        let partials = vec![PartialToolCall {
            id: String::new(),
            name: String::new(),
            arguments: "{}".to_string(),
        }];
        assert!(assemble_tool_calls(partials).is_empty());
    }

    #[test]
    fn serializes_tools_and_tool_messages() {
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![
                Message::user("read the file"),
                Message::tool_request(
                    String::new(),
                    vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"src/main.rs"}"#.to_string(),
                    }],
                ),
                Message::tool_result("call_1", "fn main() {}"),
            ],
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a project file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } }
                }),
            }],
            session_id: None,
        };
        let body = OpenAIRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
            session_id: None,
        };
        let value = serde_json::to_value(&body).unwrap();

        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "read_file");
        // The assistant tool-call turn carries no content but one function call.
        assert!(value["messages"][1]["content"].is_null());
        assert_eq!(value["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            value["messages"][1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        // The tool result is tagged with the id of the call it answers.
        assert_eq!(value["messages"][2]["role"], "tool");
        assert_eq!(value["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(value["messages"][2]["content"], "fn main() {}");
    }

    #[test]
    fn serializes_images_as_content_parts() {
        use crate::provider::ImageAttachment;
        let messages = vec![Message::user_with_images(
            "what is this?",
            vec![ImageAttachment {
                media_type: "image/png".to_string(),
                data: "QUJD".to_string(),
            }],
        )];
        let value = serde_json::to_value(wire_messages(&messages)).unwrap();

        assert_eq!(value[0]["content"][0]["type"], "text");
        assert_eq!(value[0]["content"][0]["text"], "what is this?");
        assert_eq!(value[0]["content"][1]["type"], "image_url");
        assert_eq!(
            value[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
    }

    #[test]
    fn serializes_text_only_messages_as_a_plain_string() {
        let messages = vec![Message::user("hello")];
        let value = serde_json::to_value(wire_messages(&messages)).unwrap();
        assert_eq!(value[0]["content"], "hello");
    }

    #[test]
    fn parses_plain_text_response() {
        let json = r#"{
            "choices": [{ "message": { "content": "Hi there" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4 }
        }"#;
        let response: OpenAIResponse = serde_json::from_str(json).unwrap();
        let chat = response_into_chat(response).unwrap();

        assert_eq!(chat.content, "Hi there");
        assert!(chat.tool_calls.is_empty());
        assert_eq!(chat.finish_reason, "stop");
    }

    #[test]
    fn response_without_choices_is_an_error() {
        let response: OpenAIResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert!(response_into_chat(response).is_err());
    }

    #[test]
    fn omits_tools_and_serializes_plain_messages() {
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::system("be brief"), Message::user("hi")],
            tools: Vec::new(),
        session_id: None,
        };
        let body = OpenAIRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
            session_id: None,
        };
        let value = serde_json::to_value(&body).unwrap();

        assert!(value.get("tools").is_none());
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][0]["content"], "be brief");
        assert_eq!(value["messages"][1]["content"], "hi");
    }

    #[test]
    fn chat_url_rewrites_absolute_path_against_origin() {
        let provider = OpenAIProvider::with_options(
            "k".into(),
            "https://api.orvix.id/v1".into(),
            Some("/coding/completions".into()),
            true,
        );
        assert_eq!(
            provider.chat_url(),
            "https://api.orvix.id/coding/completions"
        );
        assert_eq!(
            origin_of("https://api.orvix.id/v1"),
            Some("https://api.orvix.id".into())
        );
    }

    #[test]
    fn invalid_stream_json_is_an_error() {
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut state = StreamState::default();

        assert!(parse_event(b"data: {not json}", &sender, &mut state).is_err());
    }

    #[test]
    fn embeddings_are_reordered_by_index() {
        let response: EmbeddingsResponse = serde_json::from_str(
            r#"{"data":[{"embedding":[0.2],"index":1},{"embedding":[0.1],"index":0}]}"#,
        )
        .unwrap();

        let vectors = embeddings_into_vectors(response, 2).unwrap();

        assert_eq!(vectors, vec![vec![0.1], vec![0.2]]);
    }

    #[test]
    fn embeddings_count_mismatch_is_an_error() {
        let response: EmbeddingsResponse =
            serde_json::from_str(r#"{"data":[{"embedding":[0.1],"index":0}]}"#).unwrap();

        assert!(embeddings_into_vectors(response, 2).is_err());
    }

    #[test]
    fn parses_tool_calls_from_response() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 }
        }"#;
        let response: OpenAIResponse = serde_json::from_str(json).unwrap();
        let chat = response_into_chat(response).unwrap();

        assert_eq!(chat.finish_reason, "tool_calls");
        assert_eq!(chat.content, "");
        assert_eq!(chat.tool_calls.len(), 1);
        assert_eq!(chat.tool_calls[0].id, "call_9");
        assert_eq!(chat.tool_calls[0].name, "read_file");
        assert_eq!(chat.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(chat.usage.total_tokens, 10);
    }

    #[test]
    fn null_tool_calls_deserialize_as_empty_on_response() {
        let json = r#"{
            "choices": [{
                "message": { "content": "ok", "tool_calls": null },
                "finish_reason": "stop"
            }]
        }"#;
        let response: OpenAIResponse = serde_json::from_str(json).unwrap();
        let chat = response_into_chat(response).unwrap();
        assert_eq!(chat.content, "ok");
        assert!(chat.tool_calls.is_empty());
    }

    #[test]
    fn null_choices_deserialize_as_empty_on_response() {
        let response: OpenAIResponse = serde_json::from_str(r#"{"choices":null}"#).unwrap();
        assert!(response.choices.is_empty());
        assert!(response_into_chat(response).is_err());
    }

    #[test]
    fn null_tool_calls_deserialize_as_empty_on_stream_delta() {
        let delta: StreamDelta =
            serde_json::from_str(r#"{"content":"hi","tool_calls":null}"#).unwrap();
        assert_eq!(delta.content.as_deref(), Some("hi"));
        assert!(delta.tool_calls.is_empty());
    }
}
