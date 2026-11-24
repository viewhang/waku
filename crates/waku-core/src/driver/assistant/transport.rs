use std::future::Future;

use bytes::Bytes;
use eventsource_stream::{Event, Eventsource as _};
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use serde_json::Value;
use smol::stream::StreamExt as _;
use waku_protocol::settings::ModelProtocol;

/// Normalize provider streaming fields that the pinned Rig client cannot merge
#[derive(Clone, Debug, Default)]
pub(super) struct AssistantHttp {
    client: reqwest::Client,
    anonymous: bool,
    protocol: ModelProtocol,
}

impl AssistantHttp {
    pub(super) fn new(client: reqwest::Client, anonymous: bool, protocol: ModelProtocol) -> Self {
        Self {
            client,
            anonymous,
            protocol,
        }
    }

    fn prepare<T>(&self, mut request: Request<T>) -> Request<T> {
        if self.anonymous {
            for header in ["authorization", "x-api-key", "x-goog-api-key"] {
                request.headers_mut().remove(header);
            }
        }
        request
    }
}

impl HttpClientExt for AssistantHttp {
    fn send<T, U>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        self.client.send(self.prepare(request))
    }

    fn send_multipart<U>(
        &self,
        request: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        self.client.send_multipart(self.prepare(request))
    }

    async fn send_streaming<T>(&self, request: Request<T>) -> http_client::Result<StreamingResponse>
    where
        T: Into<Bytes> + Send,
    {
        let response = self.client.send_streaming(self.prepare(request)).await?;
        let protocol = self.protocol;
        if !matches!(
            protocol,
            ModelProtocol::OpenAiChat | ModelProtocol::AnthropicMessages
        ) {
            return Ok(response);
        }
        let (mut parts, body) = response.into_parts();
        parts.headers.remove("content-length");
        let mut cache_tokens = [None; 2];
        let body = body.eventsource().map(move |event| {
            event
                .map(|event| normalize_event(event, protocol, &mut cache_tokens))
                .map_err(|error| http_client::Error::Instance(error.into()))
        });
        Ok(Response::from_parts(parts, Box::pin(body)))
    }
}

fn normalize_event(
    event: Event,
    protocol: ModelProtocol,
    cache_tokens: &mut [Option<u64>; 2],
) -> Bytes {
    let mut data = event.data;
    if protocol == ModelProtocol::OpenAiChat
        && data.contains("\"reasoning\"")
        && let Ok(mut chunk) = serde_json::from_str::<Value>(&data)
        && let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut)
    {
        for choice in choices {
            if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut)
                && !delta
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
                && let Some(Value::String(text)) = delta.remove("reasoning")
            {
                delta.insert("reasoning_content".into(), Value::String(text));
            }
        }
        data = chunk.to_string();
    }
    if protocol == ModelProtocol::AnthropicMessages
        && (data.contains("\"message_start\"") || data.contains("\"message_delta\""))
        && let Ok(mut chunk) = serde_json::from_str::<Value>(&data)
        && merge_anthropic_cache_usage(&mut chunk, cache_tokens)
    {
        data = chunk.to_string();
    }
    let mut frame = format!("id: {}\nevent: {}\n", event.id, event.event);
    if let Some(retry) = event.retry {
        frame.push_str(&format!("retry: {}\n", retry.as_millis()));
    }
    for line in data.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    Bytes::from(frame)
}

/// Carry initial cache counts into a cumulative delta when it omits those fields
fn merge_anthropic_cache_usage(chunk: &mut Value, counts: &mut [Option<u64>; 2]) -> bool {
    const FIELDS: [&str; 2] = ["cache_creation_input_tokens", "cache_read_input_tokens"];
    match chunk["type"].as_str() {
        Some("message_start") => {
            *counts = FIELDS.map(|field| chunk["message"]["usage"][field].as_u64());
            false
        }
        Some("message_delta") => {
            let Some(usage) = chunk.get_mut("usage").and_then(Value::as_object_mut) else {
                return false;
            };
            let mut changed = false;
            for (field, count) in FIELDS.into_iter().zip(counts) {
                if let Some(current) = usage.get(field).and_then(Value::as_u64) {
                    *count = Some(current);
                } else if let Some(previous) = *count {
                    usage.insert(field.into(), previous.into());
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}
