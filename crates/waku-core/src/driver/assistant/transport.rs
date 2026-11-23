use std::future::Future;

use bytes::Bytes;
use eventsource_stream::{Event, Eventsource as _};
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use serde_json::Value;
use smol::stream::StreamExt as _;

/// Keep the pinned Rig client's OpenAI parser compatible with both reasoning field names
#[derive(Clone, Debug, Default)]
pub(super) struct AssistantHttp {
    client: reqwest::Client,
    anonymous: bool,
    normalize_reasoning: bool,
}

impl AssistantHttp {
    pub(super) fn new(client: reqwest::Client, anonymous: bool, normalize_reasoning: bool) -> Self {
        Self {
            client,
            anonymous,
            normalize_reasoning,
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
        if !self.normalize_reasoning {
            return Ok(response);
        }
        let (mut parts, body) = response.into_parts();
        parts.headers.remove("content-length");
        let body = body.eventsource().map(|event| {
            event
                .map(normalize_event)
                .map_err(|error| http_client::Error::Instance(error.into()))
        });
        Ok(Response::from_parts(parts, Box::pin(body)))
    }
}

fn normalize_event(event: Event) -> Bytes {
    let mut data = event.data;
    if data.contains("\"reasoning\"")
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
