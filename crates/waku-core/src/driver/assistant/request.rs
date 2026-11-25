//! Dispatch configured model operations while keeping chat history in Rig

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use reqwest::multipart::{Form, Part};
use rig::client::CompletionClient as _;
use rig::completion::{CompletionModel, GetTokenUsage, Usage};
use rig::message::{
    self, AssistantContent, DocumentSourceKind, Message, MimeType as _, ReasoningContent, ToolCall,
    ToolChoice, ToolResultContent, UserContent,
};
use rig::providers::{anthropic, gemini, openai};
use rig::streaming::{StreamedAssistantContent, ToolCallDeltaContent};
use serde_json::{Value, json};
use smol::stream::StreamExt as _;
use url::Url;
use waku_protocol::settings::{
    ModelKind, ModelModality, ModelProtocol, ModelPurpose, ServiceModelOptions,
};

use super::{bearer_auth, search, transport::AssistantHttp};
use crate::attachments::{AttachmentStore, AttachmentUpload, MAX_ATTACHMENT_BYTES};
use crate::blob_store::{BlobStore, is_blob_reference};
use crate::driver::DriverEventSender;
use crate::model::{ActivityItem, ActivityKind, DriverEvent, MessageAttachment};
use crate::usage_history::TokenTotals;

pub(super) struct Media {
    pub(super) mime: &'static str,
    pub(super) name: String,
    pub(super) bytes: Vec<u8>,
}

pub(super) fn media_mime(name: &str) -> Option<&'static str> {
    match name.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "wav" => Some("audio/wav"),
        "mp3" => Some("audio/mp3"),
        "aiff" | "aif" => Some("audio/aiff"),
        "aac" => Some("audio/aac"),
        "flac" => Some("audio/flac"),
        "ogg" => Some("audio/ogg"),
        "mp4" => Some("video/mp4"),
        "mov" => Some("video/mov"),
        "webm" => Some("video/webm"),
        "mpeg" | "mpg" => Some("video/mpeg"),
        _ => None,
    }
}

/// Resolve only daemon attachments selected by the user, with one combined size limit
pub(super) fn read_media(
    store: &AttachmentStore,
    blobs: &BlobStore,
    attachments: Vec<MessageAttachment>,
    options: &ServiceModelOptions,
) -> Result<Vec<Media>> {
    let mut total = 0;
    attachments
        .into_iter()
        .map(|attachment| {
            let mime =
                media_mime(&attachment.name).context("Unsupported model attachment format")?;
            let modality = if mime.starts_with("image/") {
                ModelModality::Vision
            } else if mime.starts_with("audio/") {
                ModelModality::Audio
            } else {
                ModelModality::Video
            };
            if attachment.is_dir || !options.accepts(modality) {
                bail!("The selected model and API do not accept {mime} input");
            }
            if options.protocol == ModelProtocol::OpenAiChat
                && modality == ModelModality::Audio
                && !matches!(mime, "audio/wav" | "audio/mp3")
            {
                bail!("Chat Completions audio requires WAV or MP3");
            }
            let reference = attachment
                .blob_reference
                .as_deref()
                .context("Missing managed attachment reference")?;
            // Clipboard images use content-addressed blobs and imported files use attachment IDs
            let bytes = if is_blob_reference(reference) {
                blobs.read(reference)?
            } else {
                store.read_file(reference, &attachment.path)?
            };
            total += bytes.len();
            if bytes.is_empty() || total > MAX_ATTACHMENT_BYTES {
                bail!("Model attachments are empty or exceed Waku's combined size limit");
            }
            Ok(Media {
                mime,
                name: attachment.name,
                bytes,
            })
        })
        .collect()
}

pub(super) fn message(prompt: String, media: Vec<Media>) -> Result<Message> {
    let mut content = Vec::new();
    if !prompt.trim().is_empty() || media.is_empty() {
        content.push(UserContent::text(prompt));
    }
    for asset in media {
        let data = DocumentSourceKind::Base64(
            base64::engine::general_purpose::STANDARD.encode(asset.bytes),
        );
        let part = if asset.mime.starts_with("image/") {
            UserContent::Image(message::Image {
                data,
                media_type: Some(
                    message::ImageMediaType::from_mime_type(asset.mime)
                        .context("Unsupported image encoding")?,
                ),
                detail: Some(message::ImageDetail::Auto),
                ..Default::default()
            })
        } else if asset.mime.starts_with("audio/") {
            UserContent::Audio(message::Audio {
                data,
                media_type: Some(
                    message::AudioMediaType::from_mime_type(asset.mime)
                        .context("Unsupported audio encoding")?,
                ),
                ..Default::default()
            })
        } else {
            UserContent::Video(message::Video {
                data,
                media_type: Some(
                    message::VideoMediaType::from_mime_type(asset.mime)
                        .context("Unsupported video encoding")?,
                ),
                ..Default::default()
            })
        };
        content.push(part);
    }
    Ok(Message::User {
        content: rig::OneOrMany::many(content)?,
    })
}

/// Estimate preflight tokens locally since custom gateways do not share a tokenizer
fn estimate(message: &Message) -> Result<usize> {
    Ok(match message {
        Message::User { content } => {
            let mut size = 12;
            for block in content.iter() {
                size += match block {
                    UserContent::Text(text) => text.text.len(),
                    UserContent::Image(_) => 2_048,
                    UserContent::Audio(audio) => audio.data.to_string().len().div_ceil(256),
                    UserContent::Video(video) => video.data.to_string().len().div_ceil(128),
                    other => serde_json::to_vec(other)?.len(),
                };
            }
            size
        }
        other => serde_json::to_vec(other)?.len(),
    })
}

pub(super) struct Chat<'a> {
    pub url: &'a Url,
    pub key: &'a str,
    pub model: &'a str,
    pub options: &'a ServiceModelOptions,
    pub preamble: &'a str,
    pub history: &'a [Message],
}

#[derive(Debug)]
pub(super) struct ModelReply {
    pub text: String,
    pub tokens: TokenTotals,
}

struct ChatRound {
    reply: ModelReply,
    used: u64,
    /// Preserve the provider's tool call IDs and reasoning for the follow-up request
    continuation: Option<(Message, Vec<(ToolCall, String)>)>,
}

fn chat_tokens(usage: Option<Usage>, protocol: ModelProtocol) -> TokenTotals {
    let Some(usage) = usage else {
        return TokenTotals::default();
    };
    // Anthropic reports uncached input separately, while Gemini omits thinking from output
    let uncached_input = if protocol == ModelProtocol::AnthropicMessages {
        usage.input_tokens
    } else {
        usage.input_tokens.saturating_sub(usage.cached_input_tokens)
    };
    let output = if protocol == ModelProtocol::Gemini {
        usage.output_tokens.saturating_add(usage.reasoning_tokens)
    } else {
        usage.output_tokens
    };
    TokenTotals {
        uncached_input,
        cached_input: usage.cached_input_tokens,
        cache_creation: usage.cache_creation_input_tokens,
        output,
        reasoning: usage.reasoning_tokens,
    }
}

impl Chat<'_> {
    pub async fn send(
        self,
        http: &reqwest::Client,
        prompt: Message,
        events: &DriverEventSender,
    ) -> Result<ModelReply> {
        Ok(self
            .send_with_tools(http, prompt, events, None, None)
            .await?
            .0)
    }

    pub async fn send_with_tools(
        self,
        http: &reqwest::Client,
        prompt: Message,
        events: &DriverEventSender,
        tavily: Option<&search::Tavily<'_>>,
        library: Option<&super::library_tools::AgentLibrary>,
    ) -> Result<(ModelReply, Vec<Message>)> {
        let base = self.url.as_str().trim_end_matches('/');
        let model = self.model;
        let http = AssistantHttp::new(http.clone(), self.key.is_empty(), self.options.protocol);
        match self.options.protocol {
            ModelProtocol::OpenAiChat => {
                let client = openai::CompletionsClient::builder()
                    .api_key(self.key)
                    .base_url(base)
                    .http_client(http)
                    .build()?;
                self.stream(
                    client.completion_model(model),
                    prompt,
                    events,
                    tavily,
                    library,
                )
                .await
            }
            ModelProtocol::OpenAiResponses => {
                let client = openai::Client::builder()
                    .api_key(self.key)
                    .base_url(base)
                    .http_client(http)
                    .build()?;
                self.stream(
                    client.completion_model(model),
                    prompt,
                    events,
                    tavily,
                    library,
                )
                .await
            }
            ModelProtocol::AnthropicMessages => {
                let client = anthropic::Client::builder()
                    .api_key(self.key)
                    .base_url(base.trim_end_matches("/v1"))
                    .http_client(http)
                    .build()?;
                self.stream(
                    client.completion_model(model),
                    prompt,
                    events,
                    tavily,
                    library,
                )
                .await
            }
            ModelProtocol::Gemini => {
                let model = self.model.strip_prefix("models/").unwrap_or(self.model);
                if !model
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                {
                    bail!("Invalid Gemini model ID");
                }
                let client = gemini::Client::builder()
                    .api_key(self.key)
                    .base_url(base.trim_end_matches("/v1beta").trim_end_matches("/v1"))
                    .http_client(http)
                    .build()?;
                self.stream(
                    client.completion_model(model),
                    prompt,
                    events,
                    tavily,
                    library,
                )
                .await
            }
        }
    }

    async fn stream(
        self,
        model: impl CompletionModel,
        prompt: Message,
        events: &DriverEventSender,
        tavily: Option<&search::Tavily<'_>>,
        library: Option<&super::library_tools::AgentLibrary>,
    ) -> Result<(ModelReply, Vec<Message>)> {
        let mut definitions = library
            .map(|l| l.definitions(self.options.protocol == ModelProtocol::OpenAiResponses))
            .unwrap_or_default();
        if tavily.is_some() {
            definitions.push(search::tool());
        }
        let mut history = Vec::new();
        let mut prompt = prompt;
        let mut reply = ModelReply {
            text: String::new(),
            tokens: TokenTotals::default(),
        };
        let mut web_sources = None;
        let mut final_answer = String::new();
        // A library turn can search, read several notes, then edit before composing the final answer
        let rounds = if library.is_some() { 8 } else { 1 };
        let mut used = 0;
        for round in 0..=rounds {
            let choice = (!definitions.is_empty()).then_some(if round == rounds {
                ToolChoice::None
            } else {
                ToolChoice::Auto
            });
            let result = self
                .stream_round(
                    &model,
                    prompt.clone(),
                    &history,
                    &definitions,
                    choice,
                    events,
                )
                .await?;
            used = result.used;
            reply.tokens.add(&result.reply.tokens);
            reply.text.push_str(&result.reply.text);
            let Some((assistant, calls)) = result.continuation else {
                if round > 0 && result.reply.text.trim().is_empty() {
                    bail!(tr!("assistant.search_reply_missing"));
                }
                final_answer = result.reply.text;
                break;
            };
            let results = tool_results(tavily, library, calls, &mut web_sources, events).await?;
            history.push(prompt);
            history.push(assistant);
            prompt = results;
            if !result.reply.text.trim().is_empty() {
                reply.text.push_str("\n\n");
                let _ = events.send(DriverEvent::TextDelta("\n\n".into()));
            }
        }
        if let Some(sources) = web_sources.filter(|s| !s.is_empty()) {
            let links = search::links(&sources);
            final_answer.push_str(&links);
            reply.text.push_str(&links);
            let _ = events.send(DriverEvent::TextDelta(links));
        }
        let _ = events.send(DriverEvent::UsageUpdated {
            context_tokens: (used > 0).then_some(used),
            context_window: self.options.context_window.map(u64::from),
        });
        // Keep actual tool calls and their results in order for later requests in this session
        history.push(prompt);
        if !final_answer.is_empty() {
            history.push(Message::assistant(final_answer));
        }
        Ok((reply, history))
    }

    async fn stream_round(
        &self,
        model: &impl CompletionModel,
        prompt: Message,
        extra_history: &[Message],
        tools: &[rig::completion::ToolDefinition],
        tool_choice: Option<ToolChoice>,
        events: &DriverEventSender,
    ) -> Result<ChatRound> {
        let window = self.options.context_window.map(|value| value as usize);
        let limit = [self.options.max_input_tokens, self.options.context_window]
            .into_iter()
            .flatten()
            .min();
        let mut input = self.preamble.len();
        let history = self.history.iter().chain(extra_history);
        if let Some(limit) = limit {
            for tool in tools {
                input += serde_json::to_vec(tool)?.len();
            }
            for message in history.clone().chain(std::iter::once(&prompt)) {
                input += estimate(message)?;
                if input > limit as usize {
                    bail!("Estimated input exceeds the configured token budget ({input} tokens)");
                }
            }
        }
        let configured = self.options.max_output_tokens.map(|value| value as usize);
        let output = match window {
            Some(window) => Some(configured.unwrap_or(4096).min(window.saturating_sub(input))),
            None => configured,
        };
        if output == Some(0) {
            bail!("The configured context window leaves no room for a reply");
        }
        // Keep the legacy limit field for compatible gateways that only accept it
        let completion_limit = self.options.protocol == ModelProtocol::OpenAiChat
            && (self.options.reasoning || self.url.host_str() == Some("api.openai.com"));
        let mut params = protocol_parameters(self.options, self.model, output, completion_limit)?;
        let can_call = matches!(tool_choice, Some(ToolChoice::Auto));
        if can_call
            && self.options.protocol == ModelProtocol::OpenAiResponses
            && self.options.reasoning
        {
            params["include"] = json!(["reasoning.encrypted_content"]);
        }
        let mut request = model
            .completion_request(prompt)
            .messages(history.cloned())
            .additional_params(params)
            .max_tokens_opt(
                output
                    .filter(|_| !completion_limit)
                    .map(|value| value as u64),
            );
        if !self.preamble.is_empty() {
            request = request.preamble(self.preamble.to_owned());
        }
        if let Some(choice) = tool_choice {
            for tool in tools {
                request = request.tool(tool.clone());
            }
            request = request.tool_choice(choice);
        }
        let mut stream = model.stream(request.build()).await?;
        let mut answer = String::new();
        let mut streamed_reasoning = String::new();
        let mut calls = Vec::new();
        let usage = loop {
            let Some(item) = stream.next().await else {
                bail!("The model stream ended before completing its response");
            };
            match item? {
                StreamedAssistantContent::Text(text) => {
                    answer.push_str(&text.text);
                    streamed_reasoning.clear();
                    let _ = events.send(DriverEvent::TextDelta(text.text));
                }
                StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                    streamed_reasoning.push_str(&reasoning);
                    let _ = events.send(DriverEvent::ReasoningDelta(reasoning));
                }
                StreamedAssistantContent::Reasoning(block) => {
                    // Only forward displayable text and skip bytes already sent as deltas
                    let text = block
                        .content
                        .into_iter()
                        .filter_map(|part| match part {
                            ReasoningContent::Text { text, .. }
                            | ReasoningContent::Summary(text) => Some(text),
                            _ => None,
                        })
                        .collect::<String>();
                    if streamed_reasoning.starts_with(text.as_str()) {
                        streamed_reasoning.drain(..text.len());
                    } else {
                        let unseen = text
                            .strip_prefix(streamed_reasoning.as_str())
                            .unwrap_or(&text);
                        let _ = events.send(DriverEvent::ReasoningDelta(unseen.to_owned()));
                        streamed_reasoning.clear();
                    }
                }
                StreamedAssistantContent::ToolCallDelta {
                    internal_call_id,
                    content: ToolCallDeltaContent::Name(name),
                    ..
                } if !name.is_empty() => {
                    if can_call
                        && tools.iter().any(|tool| tool.name == name)
                        && name != search::NAME
                    {
                        report_library(events, internal_call_id, &name, None, None);
                    } else if can_call
                        && name == search::NAME
                        && tools.iter().any(|tool| tool.name == name)
                    {
                        report_search(events, internal_call_id, None, None);
                    } else {
                        report_tool_request(events, internal_call_id, &name, None);
                    }
                }
                StreamedAssistantContent::ToolCall {
                    tool_call,
                    internal_call_id,
                } => {
                    if !can_call
                        || !tools
                            .iter()
                            .any(|tool| tool.name == tool_call.function.name)
                    {
                        report_tool_request(
                            events,
                            internal_call_id.clone(),
                            &tool_call.function.name,
                            Some(tool_call.function.arguments.to_string()),
                        );
                    }
                    if can_call {
                        calls.push((tool_call, internal_call_id));
                    }
                }
                StreamedAssistantContent::Final(response) => break response.token_usage(),
                _ => {}
            }
        };
        let tokens = chat_tokens(usage, self.options.protocol);
        let used = usage
            .map_or(0, |usage| usage.total_tokens)
            .max(tokens.total());
        let continuation = if calls
            .iter()
            .any(|(call, _)| tools.iter().any(|tool| tool.name == call.function.name))
        {
            // Rig builds its complete assistant message when the stream closes
            while let Some(item) = stream.next().await {
                item?;
            }
            let content = rig::OneOrMany::many(stream.choice.into_iter().filter(|item| {
                // Keep only replayable provider reasoning blocks in the follow-up request
                match (self.options.protocol, item) {
                    (ModelProtocol::OpenAiResponses, AssistantContent::Reasoning(block)) => block
                        .content
                        .iter()
                        .any(|part| matches!(part, ReasoningContent::Encrypted(_))),
                    (ModelProtocol::AnthropicMessages, AssistantContent::Reasoning(block)) => {
                        block.content.iter().any(|part| {
                            matches!(
                                part,
                                ReasoningContent::Text {
                                    signature: Some(_),
                                    ..
                                } | ReasoningContent::Redacted { .. }
                            )
                        })
                    }
                    _ => true,
                }
            }))?;
            Some((
                Message::Assistant {
                    id: stream.message_id,
                    content,
                },
                calls,
            ))
        } else {
            None
        };
        Ok(ChatRound {
            reply: ModelReply {
                text: answer,
                tokens,
            },
            used,
            continuation,
        })
    }
}

/// Answer every provider-issued tool call with its original ID before resuming the turn
async fn tool_results(
    tavily: Option<&search::Tavily<'_>>,
    library: Option<&super::library_tools::AgentLibrary>,
    calls: Vec<(ToolCall, String)>,
    sources: &mut Option<Vec<search::Source>>,
    events: &DriverEventSender,
) -> Result<Message> {
    let mut results = Vec::new();
    for (call, internal_id) in calls {
        let name = &call.function.name;
        let output = if name != search::NAME
            && let Some(library) = library
        {
            report_library(
                events,
                internal_id.clone(),
                name,
                Some(call.function.arguments.to_string()),
                None,
            );
            let value = library.call(name, call.function.arguments).await;
            let (failed, text) = match value {
                Ok(v) => (false, v),
                Err(e) => (true, format!("Tool error: {e}")),
            };
            report_library(
                events,
                internal_id,
                name,
                None,
                Some((failed, text.chars().take(900).collect())),
            );
            text
        } else if name != search::NAME || tavily.is_none() {
            tr!("assistant.tool_unavailable")
        } else if sources.is_some() {
            let message = tr!("assistant.search_limit");
            report_search(events, internal_id, None, Some((true, message.clone())));
            message
        } else {
            let query = serde_json::from_value::<search::Query>(call.function.arguments)
                .context(tr!("assistant.search_invalid_query"));
            report_search(
                events,
                internal_id.clone(),
                query.as_ref().ok().map(|q| q.query.clone()),
                None,
            );
            let found = match query {
                Ok(query) => {
                    tavily
                        .expect("web tool was offered")
                        .search(&query.query)
                        .await
                }
                Err(error) => Err(error),
            };
            let found = match found {
                Ok(found) => found,
                Err(error) => {
                    report_search(events, internal_id, None, Some((true, error.to_string())));
                    return Err(error);
                }
            };
            let output = serde_json::to_string(&found)?;
            report_search(
                events,
                internal_id,
                None,
                Some((
                    false,
                    tr!("assistant.search_result_count", count = found.len()),
                )),
            );
            *sources = Some(found);
            output
        };
        results.push(UserContent::ToolResult(message::ToolResult {
            id: call.id,
            call_id: call.call_id,
            content: rig::OneOrMany::one(ToolResultContent::text(output)),
        }));
    }
    Ok(Message::User {
        content: rig::OneOrMany::many(results)?,
    })
}

/// Keep tool progress and results attached to the provider's activity in the ordinary transcript
fn report_library(
    events: &DriverEventSender,
    id: String,
    name: &str,
    args: Option<String>,
    result: Option<(bool, String)>,
) {
    let mut item = ActivityItem::new(Some(id), ActivityKind::Tool, name, None, result.is_some())
        .with_tool_name(Some(name))
        .with_arguments(args);
    if let Some((failed, message)) = result {
        item = item.with_failed(failed).with_output(Some(message));
    }
    let _ = events.send(DriverEvent::RichActivity(item));
}

fn report_search(
    events: &DriverEventSender,
    id: String,
    query: Option<String>,
    result: Option<(bool, String)>,
) {
    let mut item = ActivityItem::new(
        Some(id),
        ActivityKind::Tool,
        tr!("assistant.web_search"),
        None,
        result.is_some(),
    )
    .with_tool_name(Some(search::NAME))
    .with_arguments(query);
    if let Some((failed, output)) = result {
        item = item.with_failed(failed).with_output(Some(output));
    }
    let _ = events.send(DriverEvent::RichActivity(item));
}

fn report_tool_request(
    events: &DriverEventSender,
    id: String,
    name: &str,
    arguments: Option<String>,
) {
    let item = ActivityItem::new(Some(id), ActivityKind::Tool, name, None, true)
        .with_tool_name(Some(name))
        .with_arguments(arguments)
        .with_failed(true)
        .with_output(Some(tr!("assistant.tool_unavailable")));
    let _ = events.send(DriverEvent::RichActivity(item));
}

fn protocol_parameters(
    options: &ServiceModelOptions,
    model: &str,
    output: Option<usize>,
    completion_limit: bool,
) -> Result<Value> {
    Ok(match options.protocol {
        ModelProtocol::OpenAiChat => {
            let mut params = json!({});
            if options.reasoning {
                params["reasoning_effort"] = json!("medium");
            }
            if let Some(output) = output.filter(|_| completion_limit) {
                params["max_completion_tokens"] = json!(output);
            }
            params
        }
        ModelProtocol::OpenAiResponses if options.reasoning => {
            json!({"store": false, "reasoning":{"effort":"medium","summary":"auto"}})
        }
        ModelProtocol::OpenAiResponses => json!({"store":false}),
        ModelProtocol::AnthropicMessages if options.reasoning => {
            if [
                "opus-4-6",
                "opus-4-7",
                "sonnet-4-6",
                "opus-4-8",
                "opus-5",
                "sonnet-5",
                "haiku-5",
                "fable",
                "mythos",
            ]
            .iter()
            .any(|family| model.contains(family))
            {
                json!({"thinking": {"type":"adaptive","display":"summarized"}})
            } else {
                let output = output.unwrap_or(2048);
                if output <= 1024 {
                    bail!("Anthropic extended thinking needs an output limit above 1,024 tokens");
                }
                json!({"thinking": {"type":"enabled","budget_tokens": (output - 1).clamp(1024, 2048)}})
            }
        }
        ModelProtocol::Gemini if options.reasoning => {
            json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}})
        }
        ModelProtocol::Gemini => json!({"generationConfig": {}}),
        _ => json!({}),
    })
}

pub(super) struct Operation<'a> {
    pub url: &'a Url,
    pub key: &'a str,
    pub model: &'a str,
    pub options: &'a ServiceModelOptions,
    pub prompt: &'a str,
    pub media: Vec<Media>,
}

impl Operation<'_> {
    pub async fn send(
        self,
        http: &reqwest::Client,
        store: Option<&AttachmentStore>,
        events: &DriverEventSender,
    ) -> Result<ModelReply> {
        let base = self.url.as_str().trim_end_matches('/');
        let estimated = self.prompt.len() + self.media.len() * 2048;
        if [self.options.context_window, self.options.max_input_tokens]
            .into_iter()
            .flatten()
            .any(|limit| estimated > limit as usize)
        {
            bail!("Estimated input exceeds the configured token budget ({estimated} tokens)");
        }
        if self.prompt.trim().is_empty() {
            bail!("Enter a prompt for the selected model");
        }
        let (endpoint, payload, label) = match self.options.purpose {
            ModelPurpose::ImageGeneration => (
                "/images/generations",
                json!({"model":self.model,"prompt":self.prompt,"n":1}),
                "Generated image",
            ),
            ModelPurpose::ImageEdit => {
                if self.media.is_empty() {
                    bail!("Attach an image to edit");
                }
                let mut form = Form::new()
                    .text("model", self.model.to_owned())
                    .text("prompt", self.prompt.to_owned());
                let field = if self.media.len() == 1 {
                    "image"
                } else {
                    "image[]"
                };
                for media in self.media {
                    form = form.part(
                        field,
                        Part::bytes(media.bytes)
                            .file_name(media.name)
                            .mime_str(media.mime)?,
                    );
                }
                let response = bearer_auth(http.post(format!("{base}/images/edits")), self.key)
                    .multipart(form)
                    .send()
                    .await?;
                return image_result(http, response, store, events, "Edited image").await;
            }
            ModelPurpose::Chat => match self.options.kind {
                ModelKind::Embedding => (
                    "/embeddings",
                    json!({"model":self.model,"input":self.prompt}),
                    "",
                ),
                ModelKind::Rerank => {
                    let mut lines = self
                        .prompt
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty());
                    let query = lines.next().context("Enter a search query")?;
                    let documents: Vec<_> = lines.collect();
                    if documents.is_empty() {
                        bail!(
                            "Put the query on the first line and one document per following line"
                        );
                    }
                    (
                        "/rerank",
                        json!({"model":self.model,"query":query,"documents":documents}),
                        "",
                    )
                }
                _ => bail!("Invalid model operation"),
            },
            ModelPurpose::CatalogOnly => bail!(tr!("service_model.unsupported_operation")),
        };
        let response = post_model_json(http, self.url, self.key, endpoint, &payload).await?;
        if !label.is_empty() {
            return image_result(http, response, store, events, label).await;
        }
        let result = read_json(response).await?;
        let key = if self.options.kind == ModelKind::Embedding {
            "data"
        } else {
            "results"
        };
        let output = result
            .get(key)
            .filter(|value| value.is_array())
            .context("The model returned no results")?;
        let text = format!(
            "\x60\x60\x60json\n{}\n\x60\x60\x60",
            serde_json::to_string_pretty(output)?
        );
        let _ = events.send(DriverEvent::TextDelta(text.clone()));
        Ok(ModelReply {
            text,
            tokens: operation_tokens(&result),
        })
    }
}

async fn image_result(
    http: &reqwest::Client,
    response: reqwest::Response,
    store: Option<&AttachmentStore>,
    events: &DriverEventSender,
    label: &str,
) -> Result<ModelReply> {
    let value = read_json(response).await?;
    let image = value["data"]
        .get(0)
        .context("The provider returned no image")?;
    let bytes = if let Some(data) = image["b64_json"].as_str() {
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("Invalid image data")?
    } else {
        let url = Url::parse(image["url"].as_str().context("Missing image data")?)?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            bail!("The provider returned an invalid image download URL");
        }
        read_body(http.get(url).send().await?, MAX_ATTACHMENT_BYTES).await?
    };
    let extension = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "webp"
    } else {
        bail!("The provider returned an unsupported image format");
    };
    let Some(store) = store else {
        return Ok(ModelReply {
            text: label.to_owned(),
            tokens: operation_tokens(&value),
        });
    };
    let name = format!(
        "{}.{}",
        label.to_ascii_lowercase().replace(' ', "-"),
        extension
    );
    let stored = store.import(
        &name,
        AttachmentUpload::File {
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    )?;
    let _ = events.send(DriverEvent::AssistantAttachment(MessageAttachment {
        path: stored.path,
        mention: name,
        name: stored.name,
        is_dir: false,
        is_image: true,
        blob_reference: Some(stored.reference),
    }));
    Ok(ModelReply {
        text: label.to_owned(),
        tokens: operation_tokens(&value),
    })
}

/// The direct image and embedding APIs return usage in their JSON response
fn operation_tokens(value: &Value) -> TokenTotals {
    let usage = &value["usage"];
    TokenTotals {
        uncached_input: usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64())
            .unwrap_or_default(),
        output: usage["output_tokens"].as_u64().unwrap_or_default(),
        ..TokenTotals::default()
    }
}

/// Direct model operations and the library share endpoint construction and bearer authentication
pub(super) async fn post_model_json(
    http: &reqwest::Client,
    url: &Url,
    key: &str,
    operation: &str,
    payload: &Value,
) -> Result<reqwest::Response> {
    let endpoint = format!("{}{operation}", url.as_str().trim_end_matches('/'));
    Ok(bearer_auth(http.post(endpoint), key)
        .json(payload)
        .send()
        .await?)
}

pub(super) async fn read_json(response: reqwest::Response) -> Result<Value> {
    Ok(serde_json::from_slice(
        &read_body(response, 2 * MAX_ATTACHMENT_BYTES).await?,
    )?)
}

async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let status = response.status();
    if !status.is_success() {
        bail!("Model request failed: HTTP {status}");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            bail!("The model response exceeds Waku's size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/j1sAAAAASUVORK5CYII=";

    struct Received {
        path: String,
        headers: String,
        body: Vec<u8>,
    }

    fn reply(mime: &'static str, body: String) -> (Url, std::thread::JoinHandle<Received>) {
        reply_with(mime, body.len(), move |socket| {
            socket.write_all(body.as_bytes()).unwrap()
        })
    }

    fn reply_with(
        mime: &'static str,
        response_length: usize,
        send_body: impl FnOnce(&mut std::net::TcpStream) + Send + 'static,
    ) -> (Url, std::thread::JoinHandle<Received>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(size) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = size.trim().parse().unwrap();
                }
                headers.push_str(&line);
            }
            let mut input = vec![0; length];
            reader.read_exact(&mut input).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nConnection: close\r\nContent-Length: {response_length}\r\n\r\n").unwrap();
            send_body(&mut socket);
            Received {
                path: request.split_whitespace().nth(1).unwrap().to_owned(),
                headers,
                body: input,
            }
        });
        (url, handle)
    }

    fn sse(events: Vec<Value>) -> String {
        events
            .into_iter()
            .map(|data| format!("data: {data}\n\n"))
            .collect()
    }

    #[test]
    fn native_catalogs_use_the_provider_path_and_key_header() {
        for (protocol, path, header, response) in [
            (
                ModelProtocol::AnthropicMessages,
                "/v1/models",
                "x-api-key",
                json!({"data":[{"id":"demo"}]}),
            ),
            (
                ModelProtocol::Gemini,
                "/v1beta/models",
                "x-goog-api-key",
                json!({"models":[{"name":"models/demo"}]}),
            ),
        ] {
            let (url, server) = reply("application/json", response.to_string());
            assert_eq!(
                super::super::list_models(url.as_str(), "test-key", protocol).unwrap(),
                ["demo"]
            );
            let received = server.join().unwrap();
            assert_eq!(received.path, path);
            assert!(
                received
                    .headers
                    .to_ascii_lowercase()
                    .contains(&format!("{header}: test-key"))
            );
        }
    }

    fn receive_chat(protocol: ModelProtocol) -> String {
        match protocol {
            ModelProtocol::OpenAiChat => {
                sse(vec![
                    json!({"choices":[{"index":0,"delta":{"reasoning_content":"Plan"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":6,"completion_tokens":2,"total_tokens":8,"prompt_tokens_details":{"cached_tokens":2}}}),
                ]) + "data: [DONE]\n\n"
            }
            ModelProtocol::OpenAiResponses => sse(vec![
                json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_test","output_index":0,"delta":"Plan","summary_index":0,"sequence_number":1}),
                json!({"type":"response.output_item.done","output_index":0,"sequence_number":2,"item":{
                    "type":"reasoning","id":"rs_test","summary":[{"type":"summary_text","text":"Plan"}],"encrypted_content":"opaque"
                }}),
                json!({"type":"response.output_text.delta","item_id":"msg_test","output_index":1,"delta":"OK","content_index":0,"sequence_number":3}),
                json!({"type":"response.completed","sequence_number":4,"response":{
                    "id":"resp_test","object":"response","created_at":0,"status":"completed","model":"demo","output":[],
                    "usage":{"input_tokens":6,"input_tokens_details":{"cached_tokens":2},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":1},"total_tokens":8}
                }}),
            ]),
            ModelProtocol::AnthropicMessages => sse(vec![
                json!({"type":"message_start","message":{"id":"msg_test","role":"assistant","content":[],"model":"demo","usage":{"input_tokens":4,"output_tokens":0,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"OK"}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ]),
            ModelProtocol::Gemini => sse(vec![json!({
                "candidates":[{"content":{"parts":[{"text":"Plan","thought":true},{"text":"OK"}],"role":"model"},"finishReason":"STOP","index":0}],
                "usageMetadata":{"promptTokenCount":6,"cachedContentTokenCount":2,"candidatesTokenCount":1,"thoughtsTokenCount":1,"totalTokenCount":8}
            })]),
        }
    }

    fn search_call_stream(protocol: ModelProtocol) -> String {
        let arguments = json!({"query":"public search terms"});
        match protocol {
            ModelProtocol::OpenAiChat => {
                sse(vec![
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_search","type":"function","function":{"name":search::NAME,"arguments":arguments.to_string()}}]},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                ]) + "data: [DONE]\n\n"
            }
            ModelProtocol::OpenAiResponses => sse(vec![
                json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":{
                    "type":"function_call","id":"fc_test","call_id":"call_search","status":"completed",
                    "name":search::NAME,"arguments":arguments.to_string()
                }}),
                json!({"type":"response.completed","sequence_number":2,"response":{
                    "id":"resp_test","object":"response","created_at":0,"status":"completed","model":"demo","output":[]
                }}),
            ]),
            ModelProtocol::AnthropicMessages => sse(vec![
                json!({"type":"message_start","message":{"id":"msg_test","role":"assistant","content":[],"model":"demo","usage":{"input_tokens":12,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_search","name":search::NAME,"input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments.to_string()}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ]),
            ModelProtocol::Gemini => sse(vec![json!({
                "candidates":[{"content":{"parts":[{"functionCall":{"name":search::NAME,"args":arguments}}],"role":"model"},"finishReason":"STOP","index":0}],
                "usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":1,"totalTokenCount":13}
            })]),
        }
    }

    #[tokio::test]
    async fn assistant_search_gives_each_protocol_results_and_appends_verified_links() {
        use super::super::tests::{mock_service, request, send};

        let http = super::super::http_client().unwrap();
        for protocol in [
            ModelProtocol::OpenAiChat,
            ModelProtocol::OpenAiResponses,
            ModelProtocol::AnthropicMessages,
            ModelProtocol::Gemini,
        ] {
            let (listener, service) = mock_service("Fixture", "demo");
            let (search_listener, search_service) = mock_service("Search fixture", "unused");
            let model_server = std::thread::spawn(move || {
                let (socket, _, first, auth) = request(&listener);
                assert!(first["tools"].to_string().contains(search::NAME), "{first}");
                assert_ne!(auth.as_deref(), Some("Bearer fixture-tavily-key"));
                send(socket, "text/event-stream", &search_call_stream(protocol));
                let (socket, _, second, _) = request(&listener);
                assert!(second.to_string().contains("Verified example"), "{second}");
                assert!(!second.to_string().contains("javascript:"));
                send(socket, "text/event-stream", &receive_chat(protocol));
                second
            });
            let search_server = std::thread::spawn(move || {
                let (socket, method, body, auth) = request(&search_listener);
                assert_eq!(method, "POST /v1/search");
                assert_eq!(auth.as_deref(), Some("Bearer fixture-tavily-key"));
                assert_eq!(
                    body,
                    json!({"query":"public search terms","max_results":5,"search_depth":"basic"})
                );
                send(socket, "application/json", &json!({"results":[
                    {"title":"Verified example","url":"https://example.org/report(a)","content":"An attributed fact"},
                    {"title":"Invalid address","url":"javascript:alert(1)","content":"discard this"}
                ]}).to_string());
            });
            let options = ServiceModelOptions {
                protocol,
                max_output_tokens: Some(2048),
                ..Default::default()
            };
            let url = Url::parse(&service.base_url).unwrap();
            let endpoint = format!("{}/search", search_service.base_url);
            let tavily = search::Tavily {
                http: &http,
                endpoint: &endpoint,
                key: "fixture-tavily-key",
            };
            let (events, received) = crate::driver::test_event_channel();
            let (reply, messages) = Chat {
                url: &url,
                key: "",
                model: "demo",
                options: &options,
                preamble: "private system prompt",
                history: &[],
            }
            .send_with_tools(
                &http,
                Message::user("private user turn"),
                &events,
                Some(&tavily),
                None,
            )
            .await
            .unwrap_or_else(|error| panic!("{protocol:?}: {error:#}"));
            assert_eq!(messages.len(), 4);
            assert!(reply.text.starts_with("OK"), "{protocol:?}: {}", reply.text);
            assert!(
                reply
                    .text
                    .contains("[Verified example](<https://example.org/report(a)>)")
            );
            assert!(received.try_iter().any(|event| matches!(event, DriverEvent::RichActivity(item) if item.complete && !item.failed && item.tool_name.as_deref() == Some(search::NAME))));
            search_server.join().unwrap();
            let second = model_server.join().unwrap();
            assert_eq!(
                second
                    .to_string()
                    .matches("https://example.org/report(a)")
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn configured_protocols_send_reasoning_limits_and_supported_media() {
        let http = super::super::http_client().unwrap();
        // Exercise the clipboard's stored reference through to each provider's HTTP payload
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(directory.path().join("attachments"));
        let blobs = BlobStore::new(directory.path().join("blobs"));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        let reference = blobs.store_image_bytes("image/png", &bytes).unwrap();
        let image = MessageAttachment {
            path: blobs.path_for(&reference).unwrap(),
            mention: "image.png".into(),
            name: "image.png".into(),
            is_dir: false,
            is_image: true,
            blob_reference: Some(reference),
        };
        for protocol in [
            ModelProtocol::OpenAiChat,
            ModelProtocol::OpenAiResponses,
            ModelProtocol::AnthropicMessages,
            ModelProtocol::Gemini,
        ] {
            let (events, received_events) = crate::driver::test_event_channel();
            let (url, server) = reply("text/event-stream", receive_chat(protocol));
            let model = if protocol == ModelProtocol::AnthropicMessages {
                "claude-opus-4-8"
            } else {
                "demo"
            };
            let options = ServiceModelOptions {
                protocol,
                reasoning: true,
                tools: false,
                max_output_tokens: Some(2048),
                context_window: Some(32000),
                max_input_tokens: Some(16000),
                inputs: vec![
                    ModelModality::Vision,
                    ModelModality::Audio,
                    ModelModality::Video,
                ],
                ..Default::default()
            };
            let mut media = read_media(&store, &blobs, vec![image.clone()], &options).unwrap();
            if protocol.supports(ModelModality::Audio) {
                media.push(Media {
                    mime: "audio/wav",
                    name: "source.wav".into(),
                    bytes: vec![4, 5, 6],
                });
            }
            if protocol.supports(ModelModality::Video) {
                media.push(Media {
                    mime: "video/mp4",
                    name: "source.mp4".into(),
                    bytes: vec![7, 8, 9],
                });
            }
            let response = Chat {
                url: &url,
                key: "example-key",
                model,
                options: &options,
                preamble: "system instruction",
                history: &[],
            }
            .send(
                &http,
                message("Look at the input".into(), media).unwrap(),
                &events,
            )
            .await
            .unwrap_or_else(|error| panic!("{protocol:?}: {error:#}"));
            assert_eq!(response.text, "OK", "{protocol:?}");
            let (cache_creation, output, reasoning) = match protocol {
                ModelProtocol::OpenAiChat => (0, 2, 0),
                ModelProtocol::OpenAiResponses | ModelProtocol::Gemini => (0, 2, 1),
                ModelProtocol::AnthropicMessages => (1, 1, 0),
            };
            assert_eq!(
                response.tokens,
                TokenTotals {
                    uncached_input: 4,
                    cached_input: 2,
                    cache_creation,
                    output,
                    reasoning,
                },
                "{protocol:?}"
            );
            let mut reasoning = String::new();
            let mut context = None;
            while let Ok(event) = received_events.try_recv() {
                match event {
                    DriverEvent::ReasoningDelta(text) => reasoning.push_str(&text),
                    DriverEvent::UsageUpdated {
                        context_tokens,
                        context_window,
                    } => context = Some((context_tokens, context_window)),
                    _ => {}
                }
            }
            assert_eq!(reasoning, "Plan", "{protocol:?}");
            assert_eq!(context, Some((Some(8), Some(32000))), "{protocol:?}");
            let received = server.join().unwrap();
            let body: Value = serde_json::from_slice(&received.body).unwrap();
            let payload = body.to_string();
            assert!(payload.contains(PNG), "{protocol:?}: missing PNG data");
            assert!(
                body.get("tools").is_none_or(
                    |tools| tools.is_null() || tools.as_array().is_some_and(Vec::is_empty)
                )
            );
            match protocol {
                ModelProtocol::OpenAiChat => {
                    assert_eq!(received.path, "/v1/chat/completions");
                    assert_eq!(body["max_completion_tokens"], 2048);
                    assert_eq!(body["reasoning_effort"], "medium");
                    assert!(payload.contains("input_audio"));
                }
                ModelProtocol::OpenAiResponses => {
                    assert_eq!(received.path, "/v1/responses");
                    assert_eq!(body["max_output_tokens"], 2048);
                    assert_eq!(body["reasoning"]["effort"], "medium");
                    assert_eq!(body["store"], false);
                    assert!(payload.contains("input_image"));
                }
                ModelProtocol::AnthropicMessages => {
                    assert_eq!(received.path, "/v1/messages");
                    assert_eq!(body["max_tokens"], 2048);
                    assert_eq!(body["thinking"]["type"], "adaptive");
                    assert_eq!(body["thinking"]["display"], "summarized");
                    assert!(
                        received
                            .headers
                            .to_ascii_lowercase()
                            .contains("x-api-key: example-key")
                    );
                }
                ModelProtocol::Gemini => {
                    assert!(
                        received
                            .path
                            .starts_with("/v1beta/models/demo:streamGenerateContent")
                    );
                    assert_eq!(body["generationConfig"]["maxOutputTokens"], 2048);
                    assert_eq!(
                        body["generationConfig"]["thinkingConfig"]["includeThoughts"],
                        true
                    );
                    assert!(payload.contains("audio/wav"));
                    assert!(payload.contains("video/mp4"));
                }
            }
        }
    }

    #[tokio::test]
    async fn shows_complete_reasoning_and_unavailable_tool_requests_in_order() {
        let function = json!({"type":"function_call","id":"fc_test","call_id":"call_test",
            "name":"get_status","status":"in_progress","arguments":""});
        let mut completed = function.clone();
        completed["status"] = json!("completed");
        completed["arguments"] = json!(r#"{"job":"example"}"#);
        let before_answer = sse(vec![
            json!({"type":"response.output_item.done","output_index":0,"sequence_number":1,"item":{
                "type":"reasoning","id":"rs_test","summary":[{"type":"summary_text","text":"Check status"}],"encrypted_content":"opaque"
            }}),
            json!({"type":"response.output_item.added","output_index":1,"sequence_number":2,"item":function}),
        ]);
        let answer = sse(vec![
            json!({"type":"response.output_item.done","output_index":1,"sequence_number":3,"item":completed}),
            json!({"type":"response.output_text.delta","item_id":"msg_test","output_index":2,"delta":"OK","content_index":0,"sequence_number":4}),
            json!({"type":"response.completed","sequence_number":5,"response":{
                "id":"resp_test","object":"response","created_at":0,"status":"completed","model":"demo","output":[]
            }}),
        ]);
        let timeout = std::time::Duration::from_secs(5);
        let (release, wait) = std::sync::mpsc::channel();
        let (url, server) = reply_with(
            "text/event-stream",
            before_answer.len() + answer.len(),
            move |socket| {
                socket.write_all(before_answer.as_bytes()).unwrap();
                // The server withholds the answer until the earlier events are delivered
                wait.recv_timeout(timeout).unwrap();
                socket.write_all(answer.as_bytes()).unwrap();
            },
        );
        let http = super::super::http_client().unwrap();
        let (events, received) = crate::driver::test_event_channel();
        let observed = std::thread::spawn(move || {
            assert!(
                matches!(received.recv_timeout(timeout).unwrap(), DriverEvent::ReasoningDelta(text) if text == "Check status")
            );
            let DriverEvent::RichActivity(requested) = received.recv_timeout(timeout).unwrap()
            else {
                panic!("Expected a tool request before the answer");
            };
            assert_eq!(requested.tool_name.as_deref(), Some("get_status"));
            assert!(requested.failed && requested.complete);
            assert!(received.try_recv().is_err());
            release.send(()).unwrap();
            (requested, received)
        });
        let options = ServiceModelOptions {
            protocol: ModelProtocol::OpenAiResponses,
            ..Default::default()
        };
        let response = Chat {
            url: &url,
            key: "test-key",
            model: "demo",
            options: &options,
            preamble: "",
            history: &[],
        }
        .send(&http, Message::user("Check the job"), &events)
        .await
        .unwrap();
        assert_eq!(response.text, "OK");
        let (requested, received) = observed.join().unwrap();
        assert!(
            matches!(received.try_recv().unwrap(), DriverEvent::TextDelta(text) if text == "OK")
        );
        let DriverEvent::RichActivity(with_arguments) = received.try_recv().unwrap() else {
            panic!("Expected complete tool arguments");
        };
        assert_eq!(with_arguments.source_id, requested.source_id);
        assert_eq!(
            with_arguments.arguments.as_deref(),
            Some(r#"{"job":"example"}"#)
        );
        assert_eq!(
            with_arguments.output,
            Some(tr!("assistant.tool_unavailable"))
        );
        assert!(matches!(
            received.try_recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: None,
                context_window: None,
            }
        ));
        assert!(received.try_recv().is_err());
        let wire: Value = serde_json::from_slice(&server.join().unwrap().body).unwrap();
        assert_ne!(wire["reasoning"]["summary"], "auto");
    }

    #[tokio::test]
    async fn streams_reasoning_from_an_http_hostname_without_credentials() {
        let first = sse(vec![
            json!({"choices":[{"delta":{"reasoning":"Plan"},"finish_reason":null}]}),
        ]);
        let rest = sse(vec![
            json!({"choices":[{"delta":{"content":"OK"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ]) + "data: [DONE]\n\n";
        let (release, wait) = std::sync::mpsc::channel();
        let timeout = std::time::Duration::from_secs(5);
        let (local_url, server) = reply_with(
            "text/event-stream",
            first.len() + rest.len(),
            move |socket| {
                socket.write_all(first.as_bytes()).unwrap();
                wait.recv_timeout(timeout).unwrap();
                socket.write_all(rest.as_bytes()).unwrap();
            },
        );
        let address = local_url.socket_addrs(|| None).unwrap()[0];
        let mut url = local_url;
        url.set_host(Some("models.test")).unwrap();
        let http = reqwest::Client::builder()
            .no_proxy()
            .resolve("models.test", address)
            .build()
            .unwrap();
        let (events, received) = crate::driver::test_event_channel();
        let observed = std::thread::spawn(move || {
            assert!(
                matches!(received.recv_timeout(timeout).unwrap(), DriverEvent::ReasoningDelta(text) if text == "Plan")
            );
            release.send(()).unwrap();
            assert!(matches!(
                received.recv_timeout(timeout).unwrap(),
                DriverEvent::TextDelta(text) if text == "OK"
            ));
            assert!(matches!(
                received.recv_timeout(timeout).unwrap(),
                DriverEvent::UsageUpdated {
                    context_tokens: None,
                    context_window: None,
                }
            ));
        });
        let options = ServiceModelOptions {
            reasoning: true,
            ..Default::default()
        };
        let answer = Chat {
            url: &url,
            key: "",
            model: "Qwen3.8-27B",
            options: &options,
            preamble: "",
            history: &[],
        }
        .send(&http, Message::user("Test reasoning"), &events)
        .await
        .unwrap();
        observed.join().unwrap();
        assert_eq!(answer.text, "OK");
        let received = server.join().unwrap();
        let headers = received.headers.to_ascii_lowercase();
        assert!(headers.contains(&format!("host: models.test:{}", address.port())));
        assert!(!headers.contains("authorization:"));
    }

    #[tokio::test]
    async fn token_budgets_reject_input_and_adjust_the_wire_output_cap() {
        let http = super::super::http_client().unwrap();
        let (events, _) = crate::driver::test_event_channel();
        let (url, server) = reply("text/event-stream", receive_chat(ModelProtocol::OpenAiChat));
        let options = ServiceModelOptions {
            context_window: Some(100),
            max_input_tokens: Some(18),
            max_output_tokens: Some(90),
            ..Default::default()
        };
        let make_chat = |options| Chat {
            url: &url,
            key: "example-key",
            model: "demo",
            options,
            preamble: "",
            history: &[],
        };
        let error = make_chat(&options)
            .send(&http, Message::user("1234567"), &events)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("configured token budget"));
        let relaxed = ServiceModelOptions {
            max_input_tokens: Some(40),
            ..options.clone()
        };
        make_chat(&relaxed)
            .send(&http, Message::user("1234567"), &events)
            .await
            .unwrap();
        let sent: Value = serde_json::from_slice(&server.join().unwrap().body).unwrap();
        assert_eq!(sent["max_tokens"], 81);
    }

    #[tokio::test]
    async fn generation_edits_embeddings_and_rerank_dispatch_to_their_endpoints() {
        let http = super::super::http_client().unwrap();
        let root = std::env::temp_dir().join(format!("waku-model-files-{}", uuid::Uuid::new_v4()));
        let store = AttachmentStore::new(root.clone());
        let blobs = BlobStore::new(root.join("blobs"));
        let (events, received_events) = crate::driver::test_event_channel();
        for (purpose, kind, endpoint, prompt) in [
            (
                ModelPurpose::ImageGeneration,
                ModelKind::Image,
                "/v1/images/generations",
                "Draw a square",
            ),
            (
                ModelPurpose::ImageEdit,
                ModelKind::Image,
                "/v1/images/edits",
                "Make it red",
            ),
            (
                ModelPurpose::Chat,
                ModelKind::Embedding,
                "/v1/embeddings",
                "Embed this",
            ),
            (
                ModelPurpose::Chat,
                ModelKind::Rerank,
                "/v1/rerank",
                "red\nred square\nblue circle",
            ),
        ] {
            let options = ServiceModelOptions {
                purpose,
                kind,
                ..Default::default()
            };
            let response = match kind {
                ModelKind::Image => json!({"data":[{"b64_json":PNG}]}),
                ModelKind::Embedding => json!({"data":[{"embedding":[0.25,0.75]}]}),
                ModelKind::Rerank => json!({"results":[{"index":0,"relevance_score":0.9}]}),
                _ => unreachable!(),
            };
            let (url, server) = reply("application/json", response.to_string());
            let mut media = Vec::new();
            if purpose == ModelPurpose::ImageEdit {
                let file = store
                    .import(
                        "source.png",
                        AttachmentUpload::File {
                            data_base64: PNG.into(),
                        },
                    )
                    .unwrap();
                let attachment = MessageAttachment {
                    path: file.path,
                    mention: file.name.clone(),
                    name: file.name,
                    is_dir: false,
                    is_image: true,
                    blob_reference: Some(file.reference),
                };
                assert!(
                    read_media(
                        &store,
                        &blobs,
                        vec![attachment.clone()],
                        &ServiceModelOptions::default()
                    )
                    .is_err()
                );
                media = read_media(&store, &blobs, vec![attachment], &options).unwrap();
            }
            Operation {
                url: &url,
                key: "example-key",
                model: "demo",
                options: &options,
                prompt,
                media,
            }
            .send(&http, Some(&store), &events)
            .await
            .unwrap();
            let received = server.join().unwrap();
            assert_eq!(received.path, endpoint);
            if purpose == ModelPurpose::ImageEdit {
                let body = String::from_utf8_lossy(&received.body);
                assert!(received.headers.contains("multipart/form-data"));
                assert!(body.contains("filename=\"source.png\"") && body.contains("Make it red"));
                assert!(
                    received
                        .body
                        .windows(8)
                        .any(|part| part == b"\x89PNG\r\n\x1a\n")
                );
            } else {
                let sent: Value = serde_json::from_slice(&received.body).unwrap();
                assert_eq!(sent["model"], "demo");
                match kind {
                    ModelKind::Image => assert_eq!(sent["prompt"], prompt),
                    ModelKind::Embedding => assert_eq!(sent["input"], prompt),
                    ModelKind::Rerank => {
                        assert_eq!(sent["documents"], json!(["red square", "blue circle"]))
                    }
                    _ => unreachable!(),
                }
            }
            if kind == ModelKind::Image {
                let DriverEvent::AssistantAttachment(image) = received_events.try_recv().unwrap()
                else {
                    panic!("Missing image attachment")
                };
                assert_eq!(
                    store
                        .read_file(image.blob_reference.as_deref().unwrap(), &image.path)
                        .unwrap(),
                    base64::engine::general_purpose::STANDARD
                        .decode(PNG)
                        .unwrap()
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
