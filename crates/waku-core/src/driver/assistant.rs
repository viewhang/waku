//! Direct model sessions stream replies through Rig

mod request;
mod transport;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::attachments::AttachmentStore;
use anyhow::{Context as _, anyhow, bail};
use rig::message::Message as RigMessage;
use serde::Deserialize;
use tokio::sync::mpsc;
use url::Url;
use uuid::Uuid;
use waku_protocol::TranslationLanguage;
use waku_protocol::settings::{
    AssistantProfile, ModelKind, ModelProtocol, ModelPurpose, ModelService, ServiceModel,
    ServiceModelOptions,
};

use super::{DriverControl, DriverEventSender, DriverHandle};
use crate::model::{DriverEvent, Message, MessageAttachment, MessageRole, ProviderResumeCursor};
use crate::settings::DaemonSettingsStore;

const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// Use a canonical service URL both for requests and for credential lookup
pub(crate) fn service_url(base_url: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(base_url.trim()).context("Invalid model service URL")?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.scheme(), "http" | "https")
    {
        bail!("Use an HTTP or HTTPS model service URL without credentials, a query, or a fragment");
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

fn api_key(
    url: &Url,
    service: Option<&ModelService>,
    settings: &DaemonSettingsStore,
) -> anyhow::Result<String> {
    if let Some(service) = service
        && let Some(key) = settings.model_service_key(service.id, url.as_str())?
    {
        return Ok(key);
    }

    default_api_key(url)
}

fn draft_api_key(url: &Url, key: &str) -> anyhow::Result<String> {
    if key.is_empty() {
        return default_api_key(url);
    }
    if !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("The API key must be a single token");
    }
    Ok(key.to_owned())
}

fn default_api_key(url: &Url) -> anyhow::Result<String> {
    if url.scheme() == "http" || is_loopback(url) {
        return Ok(String::new());
    }
    // Only send the environment's OpenAI key over HTTPS to its official host
    if url.scheme() == "https"
        && url.host_str() == Some("api.openai.com")
        && let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Ok(key);
    }
    bail!("Set an API key for this model service in Settings → Model Services");
}

fn bearer_auth(request: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
    if key.is_empty() {
        request
    } else {
        request.bearer_auth(key)
    }
}

pub(crate) fn test_model(base_url: &str, key: &str, model: &ServiceModel) -> anyhow::Result<()> {
    if model.id.trim().is_empty() {
        bail!("Enter the model ID first");
    }
    if !model.options.valid_token_limits() {
        bail!("Invalid token limits");
    }
    let url = service_url(base_url)?;
    let key = draft_api_key(&url, key)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let http = http_client()?;
    let (wake, _) = smol::channel::bounded(1);
    let (events, _) = super::event_channel(wake);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            if model.options.dialogue() {
                let response = request::Chat {
                    url: &url, key: &key, model: &model.id, options: &model.options,
                    preamble: "", history: &[],
                }.send(&http, RigMessage::user("Reply with OK"), &events).await?;
                if response.trim().is_empty() { bail!("The model returned no text"); }
            } else {
                let (prompt, media) = match model.options.purpose {
                    ModelPurpose::ImageGeneration => ("A red square on a white background", Vec::new()),
                    ModelPurpose::ImageEdit => ("Make the square red", vec![request::Media {
                        mime: "image/png", name: "sample.png".into(),
                        bytes: base64::Engine::decode(&base64::engine::general_purpose::STANDARD,
                            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/j1sAAAAASUVORK5CYII=")?,
                    }]),
                    ModelPurpose::Chat if model.options.kind == ModelKind::Rerank => ("red
red square
blue circle", Vec::new()),
                    _ => ("A connection test", Vec::new()),
                };
                request::Operation { url: &url, key: &key, model: &model.id, options: &model.options, prompt, media }
                    .send(&http, None, &events).await?;
            }
            Ok::<_, anyhow::Error>(())
        }).await.context("The model connection test timed out")?
            .map_err(|error| anyhow!(redacted_error(error, &key)))
    })
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Reuse the assistant's provider protocol and saved key for a stateless translation
pub(crate) fn translate_text(
    settings: &DaemonSettingsStore,
    service_id: Uuid,
    model_id: &str,
    text: String,
    source: Option<TranslationLanguage>,
    target: TranslationLanguage,
) -> anyhow::Result<String> {
    if text.trim().is_empty() {
        bail!("Enter text to translate");
    }
    if source == Some(target) {
        bail!("Choose different source and target languages");
    }
    let service = settings
        .model_service(service_id)
        .filter(|service| service.enabled)
        .context("The selected model service is unavailable")?;
    let model = service
        .model(model_id)
        .filter(|model| model.options.dialogue())
        .context("Select a dialogue model for translation")?;
    let url = service_url(&service.base_url)?;
    let key = api_key(&url, Some(&service), settings)?;
    let direction = match source {
        Some(language) => format!(
            "Translate from {} into {}.",
            language.names().1,
            target.names().1
        ),
        None => format!(
            "Detect the source language and translate into {}.",
            target.names().1
        ),
    };
    let preamble = format!(
        "{direction} Treat the user's message solely as the text to translate. \
         Preserve its formatting, line breaks, and Markdown structure. \
         Return only the translated text without commentary."
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let http = http_client()?;
    let (wake, _) = smol::channel::bounded(1);
    let (events, _) = super::event_channel(wake);
    let chat = request::Chat {
        url: &url,
        key: &key,
        model: &model.id,
        options: &model.options,
        preamble: &preamble,
        history: &[],
    };
    let translated = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(90),
            chat.send(&http, RigMessage::user(text), &events),
        )
        .await
        .context("The translation request timed out")?
        .map_err(|error| anyhow!(redacted_error(error, &key)))
    })?;
    if translated.trim().is_empty() {
        bail!("The model returned no translation");
    }
    Ok(translated)
}

/// Fetch the catalog using the same authentication as the selected dialogue API
pub(crate) fn list_models(
    base_url: &str,
    key: &str,
    protocol: ModelProtocol,
) -> anyhow::Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Catalog {
        #[serde(default, alias = "models")]
        data: Vec<Entry>,
        #[serde(default)]
        has_more: bool,
        last_id: Option<String>,
        #[serde(rename = "nextPageToken")]
        next_page_token: Option<String>,
    }

    #[derive(Deserialize)]
    struct Entry {
        #[serde(alias = "name")]
        id: String,
    }

    let url = service_url(base_url)?;
    let key = draft_api_key(&url, key)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), async {
            let client = http_client()?;
            let base = url.as_str().trim_end_matches('/');
            let (mut request, cursor_param) = match protocol {
                ModelProtocol::AnthropicMessages => (
                    client
                        .get(format!("{}/v1/models", base.trim_end_matches("/v1")))
                        .header("anthropic-version", "2023-06-01"),
                    "after_id",
                ),
                ModelProtocol::Gemini => (
                    client.get(format!(
                        "{}/v1beta/models",
                        base.trim_end_matches("/v1beta").trim_end_matches("/v1")
                    )),
                    "pageToken",
                ),
                _ => (client.get(format!("{base}/models")), ""),
            };
            if !key.is_empty() {
                request = match protocol {
                    ModelProtocol::AnthropicMessages => request.header("x-api-key", &key),
                    ModelProtocol::Gemini => request.header("x-goog-api-key", &key),
                    _ => request.bearer_auth(&key),
                };
            }
            let mut models = Vec::new();
            let mut page = None::<String>;
            loop {
                let mut request = request
                    .try_clone()
                    .context("Invalid model catalog request")?
                    .build()?;
                if let Some(cursor) = &page {
                    request
                        .url_mut()
                        .query_pairs_mut()
                        .append_pair(cursor_param, cursor);
                }
                let response = client.execute(request).await?;
                if !response.status().is_success() {
                    bail!(
                        "Model catalog request failed: HTTP {}",
                        response.status().as_u16()
                    );
                }
                let catalog: Catalog = response
                    .json()
                    .await
                    .context("Invalid model catalog response")?;
                models.extend(
                    catalog
                        .data
                        .into_iter()
                        .map(|entry| {
                            if protocol == ModelProtocol::Gemini {
                                entry
                                    .id
                                    .strip_prefix("models/")
                                    .unwrap_or(&entry.id)
                                    .to_owned()
                            } else {
                                entry.id
                            }
                        })
                        .filter(|id| !id.trim().is_empty()),
                );
                let next = match protocol {
                    ModelProtocol::AnthropicMessages if catalog.has_more => catalog.last_id,
                    ModelProtocol::Gemini => catalog.next_page_token,
                    _ => None,
                };
                let Some(next) = next.filter(|next| Some(next) != page.as_ref()) else {
                    break;
                };
                page = Some(next);
            }
            models.sort();
            models.dedup();
            Ok::<_, anyhow::Error>(models)
        })
        .await
        .context("The model catalog request timed out")?
        .map_err(|error| anyhow!(redacted_error(error, &key)))
    })
}

fn redacted_error(error: anyhow::Error, key: &str) -> String {
    let message = error.to_string();
    let message = if key.is_empty() {
        message
    } else {
        message.replace(key, "[redacted]")
    };
    message.chars().take(500).collect()
}

enum Control {
    Prompt(String, Vec<MessageAttachment>),
    Cancel,
    ClearContext,
}

/// Closing the last handle drops the channel and cancels any active model stream
struct AssistantDriver {
    commands: mpsc::UnboundedSender<Control>,
    /// Counts a queued prompt as busy before the worker starts its request
    busy: Arc<AtomicBool>,
}

impl DriverControl for AssistantDriver {
    fn prompt(&self, prompt: String) {
        self.prompt_with_attachments(prompt, Vec::new());
    }

    fn prompt_with_attachments(&self, prompt: String, attachments: Vec<MessageAttachment>) {
        self.busy.store(true, Ordering::Release);
        let _ = self.commands.send(Control::Prompt(prompt, attachments));
    }

    fn cancel(&self) {
        let _ = self.commands.send(Control::Cancel);
    }

    fn assistant_context_is_idle(&self) -> bool {
        self.commands.is_closed() || !self.busy.load(Ordering::Acquire)
    }

    fn clear_assistant_context(&self) {
        let _ = self.commands.send(Control::ClearContext);
    }

    fn respond(&self, _: String, _: String) {}

    fn rollback(&self, _: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        bail!("Rewind is not available for direct model sessions");
    }
}

pub(crate) fn start(
    profile: AssistantProfile,
    settings: Arc<DaemonSettingsStore>,
    attachments: AttachmentStore,
    messages: Vec<Message>,
    events: DriverEventSender,
) -> anyhow::Result<DriverHandle> {
    if profile.name.trim().is_empty() || profile.model.trim().is_empty() {
        bail!("An assistant name and model ID are required");
    }
    connection_settings(&profile, &settings)?;
    let history = messages
        .into_iter()
        .filter_map(|message| {
            let text = message.display_content.unwrap_or(message.content);
            // Image-only rows have no text to replay when the provider changes
            if text.trim().is_empty() {
                return None;
            }
            match message.role {
                MessageRole::User => Some(RigMessage::user(text)),
                MessageRole::Assistant => Some(RigMessage::assistant(text)),
                _ => None,
            }
        })
        .collect();
    let (tx, rx) = mpsc::unbounded_channel();
    let busy = Arc::new(AtomicBool::new(false));
    let worker_busy = busy.clone();
    std::thread::Builder::new()
        .name("waku-assistant".into())
        .spawn(move || {
            if let Err(error) = run(
                profile,
                settings,
                attachments,
                history,
                rx,
                &worker_busy,
                &events,
            ) {
                let _ = events.send(DriverEvent::Error(error.to_string()));
                let _ = events.send(DriverEvent::ProcessExited);
            }
        })
        .context("Could not start the assistant worker")?;
    Ok(DriverHandle::from_control(Arc::new(AssistantDriver {
        commands: tx,
        busy,
    })))
}

/// Resolve each turn against current settings so disabling a service also stops reuse
fn connection_settings(
    profile: &AssistantProfile,
    settings: &DaemonSettingsStore,
) -> anyhow::Result<(Url, Option<ModelService>)> {
    let service = match profile.model_service_id {
        Some(id) => Some(
            settings
                .model_service(id)
                .context("Model service is missing")?,
        ),
        None => settings.legacy_model_service(profile.id, &profile.base_url),
    };
    if let Some(service) = service.as_ref() {
        if !service.enabled {
            bail!("Model service {} is disabled", service.name);
        }
    }
    let base_url = service
        .as_ref()
        .map_or(profile.base_url.as_str(), |service| &service.base_url);
    Ok((service_url(base_url)?, service))
}

fn run(
    profile: AssistantProfile,
    settings: Arc<DaemonSettingsStore>,
    attachments: AttachmentStore,
    mut history: Vec<RigMessage>,
    mut commands: mpsc::UnboundedReceiver<Control>,
    busy: &AtomicBool,
    events: &DriverEventSender,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let http = http_client()?;
        let legacy_options = ServiceModelOptions::default();
        while let Some(command) = commands.recv().await {
            let (prompt, input) = match command {
                Control::Prompt(prompt, input) => (prompt, input),
                Control::ClearContext => {
                    history.clear();
                    continue;
                }
                Control::Cancel => continue,
            };
            let _ = events.send(DriverEvent::TurnStarted);
            let request = async {
                let (url, service) = connection_settings(&profile, &settings)?;
                let (model, options) = if let Some(service) = &service {
                    let model = service.model(&profile.model).with_context(|| {
                        tr!(
                            "assistant.model_unavailable",
                            model = profile.model.as_str()
                        )
                    })?;
                    (model.id.as_str(), &model.options)
                } else {
                    (profile.model.as_str(), &legacy_options)
                };
                let key = api_key(&url, service.as_ref(), &settings)?;
                let result = async {
                    let media = request::read_media(&attachments, input, options)?;
                    if options.dialogue() {
                        let prompt = request::message(prompt, media)?;
                        let answer = request::Chat {
                            url: &url,
                            key: &key,
                            model,
                            options,
                            preamble: &profile.system_prompt,
                            history: &history,
                        }
                        .send(&http, prompt.clone(), events)
                        .await?;
                        let mut messages = vec![prompt];
                        if !answer.is_empty() {
                            messages.push(RigMessage::assistant(&answer));
                        }
                        Ok::<_, anyhow::Error>((answer, messages))
                    } else {
                        let summary = request::Operation {
                            url: &url,
                            key: &key,
                            model,
                            options,
                            prompt: &prompt,
                            media,
                        }
                        .send(&http, Some(&attachments), events)
                        .await?;
                        Ok((summary, Vec::new()))
                    }
                }
                .await;
                result.map_err(|error| anyhow!(redacted_error(error, &key)))
            };
            let result = tokio::select! {
                result = tokio::time::timeout(TURN_TIMEOUT, request) => result,
                command = commands.recv() => {
                    busy.store(false, Ordering::Release);
                    if command.is_none() {
                        break;
                    }
                    continue;
                }
            };
            busy.store(false, Ordering::Release);
            match result {
                Ok(Ok((summary, messages))) => {
                    history.extend(messages);
                    let _ = events.send(DriverEvent::TurnFinished {
                        success: true,
                        summary: Some(summary),
                    });
                }
                failure => {
                    let message = match failure {
                        Ok(Err(error)) => error.to_string(),
                        Err(_) => "The model turn timed out".into(),
                        Ok(Ok(_)) => unreachable!(),
                    };
                    let _ = events.send(DriverEvent::TurnFinished {
                        success: false,
                        summary: Some(message.chars().take(500).collect()),
                    });
                }
            }
        }
        Ok(())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::Path;

    #[test]
    fn model_service_accepts_http_on_the_network() {
        let domain = service_url("http://models.test:8080/v1/").unwrap();
        assert_eq!(domain.as_str(), "http://models.test:8080/v1");
        assert_eq!(draft_api_key(&domain, "").unwrap(), "");
        let endpoint = service_url("http://192.168.1.50:8080/v1/").unwrap();
        assert_eq!(endpoint.as_str(), "http://192.168.1.50:8080/v1");
        assert!(!is_loopback(&endpoint));
        assert!(service_url("http://user:pass@192.168.1.50:8080/v1").is_err());
        assert!(service_url("ftp://192.168.1.50/v1").is_err());
    }

    #[test]
    fn translation_uses_the_selected_service_and_only_the_supplied_text() {
        let (listener, service) = mock_service("Translation fixture", "text-model");
        let project = std::env::temp_dir().join(format!("waku-translation-{}", Uuid::new_v4()));
        let settings = DaemonSettingsStore::open(project.join("settings.json")).unwrap();
        settings
            .save_model_service(service.clone(), Some("fixture-translation-key"))
            .unwrap();
        assert!(
            translate_text(
                &settings,
                service.id,
                "text-model",
                "hello".into(),
                Some(TranslationLanguage::English),
                TranslationLanguage::English,
            )
            .is_err()
        );
        let provider = std::thread::spawn(move || {
            let (socket, method, body, auth) = request(&listener);
            assert_eq!(method, "POST /v1/chat/completions");
            assert_eq!(auth.as_deref(), Some("Bearer fixture-translation-key"));
            assert_eq!(body["model"], "text-model");
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0]["role"], "system");
            assert!(
                messages[0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Detect the source language and translate into Japanese")
            );
            assert_eq!(messages[1]["role"], "user");
            assert_eq!(messages[1]["content"], "Hello\nworld");
            assert!(body["tools"].is_null() || body["tools"].as_array().is_some_and(Vec::is_empty));
            stream(socket, json!({"content": "こんにちは\n世界"}), "stop");
        });
        assert_eq!(
            translate_text(
                &settings,
                service.id,
                "text-model",
                "Hello\nworld".into(),
                None,
                TranslationLanguage::Japanese
            )
            .unwrap(),
            "こんにちは\n世界",
        );
        provider.join().unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    /// Verify discovery, conversations without built-in tools and history at a different endpoint
    #[test]
    fn switches_services_and_keeps_conversation_history() {
        let (first, first_service) = mock_service("first", "first-model");
        let (second, second_service) = mock_service("second", "second-model");
        let first_server = std::thread::spawn(move || {
            let (socket, method, _, auth) = request(&first);
            assert_eq!(auth.as_deref(), Some("Bearer draft-key"));
            assert_eq!(method, "GET /v1/models");
            send(
                socket,
                "application/json",
                r#"{"data":[{"id":"first-model"},{"id":"first-model"},{"id":"other"}]}"#,
            );
            let (socket, method, body, auth) = request(&first);
            assert_eq!(method, "POST /v1/chat/completions");
            assert_eq!(auth.as_deref(), Some("Bearer modified-key"));
            assert_eq!(body["model"], "first-model");
            stream(socket, json!({"content": "OK"}), "stop");
            let (socket, method, body, auth) = request(&first);
            assert_eq!(auth.as_deref(), Some("Bearer first-saved-key"));
            assert_eq!(method, "POST /v1/chat/completions");
            assert_eq!(body["model"], "first-model");
            assert_eq!(
                body["messages"][0]["content"][0]["text"],
                "Answer concisely"
            );
            assert!(body["tools"].is_null() || body["tools"].as_array().is_some_and(Vec::is_empty));
            stream(
                socket,
                json!({"content": "Start with run demo-check."}),
                "stop",
            );
        });
        let second_server = std::thread::spawn(move || {
            let (socket, method, body, auth) = request(&second);
            assert_eq!(auth.as_deref(), Some("Bearer second-saved-key"));
            assert_eq!(method, "POST /v1/chat/completions");
            assert_eq!(body["model"], "second-model");
            assert_eq!(
                body["messages"][0]["content"][0]["text"],
                "Answer concisely"
            );
            for (index, role, expected) in [
                (1, "user", "How do I start?"),
                (2, "assistant", "Start with run demo-check."),
                (3, "user", "What was the command?"),
            ] {
                let message = &body["messages"][index];
                let content = &message["content"];
                assert_eq!(message["role"], role);
                assert_eq!(
                    content.as_str().or_else(|| content[0]["text"].as_str()),
                    Some(expected)
                );
            }
            stream(socket, json!({"content": "run demo-check"}), "stop");
        });

        let project = std::env::temp_dir().join(format!("waku-assistant-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&project).unwrap();
        let settings_path = project.join("settings.json");
        let settings = DaemonSettingsStore::open(settings_path.clone()).unwrap();
        assert_eq!(
            list_models(
                &first_service.base_url,
                "draft-key",
                ModelProtocol::OpenAiChat
            )
            .unwrap(),
            ["first-model", "other"]
        );
        test_model(
            &first_service.base_url,
            "modified-key",
            &ServiceModel::new("first-model".into()),
        )
        .unwrap();
        assert!(settings.get().model_services.is_empty());
        settings
            .save_model_service(first_service.clone(), Some("first-saved-key"))
            .unwrap();
        let mut second_service = second_service;
        second_service.models[0]
            .aliases
            .push("second-original".into());
        settings
            .save_model_service(second_service.clone(), Some("second-saved-key"))
            .unwrap();
        settings
            .replace_preferences(crate::DaemonSettings::default())
            .unwrap();
        drop(settings);
        let settings = Arc::new(DaemonSettingsStore::open(settings_path).unwrap());

        let mut profile = AssistantProfile {
            id: uuid::Uuid::new_v4(),
            name: "Guide".into(),
            system_prompt: "Answer concisely".into(),
            model: "first-model".into(),
            model_service_id: Some(first_service.id),
            base_url: String::new(),
        };
        let answer = prompt(
            profile.clone(),
            settings.clone(),
            &project,
            Vec::new(),
            "How do I start?",
        );
        assert_eq!(answer, "Start with run demo-check.");

        profile.model_service_id = Some(second_service.id);
        profile.model = "second-original".into();
        let follow_up = prompt(
            profile,
            settings,
            &project,
            vec![
                Message::new(MessageRole::User, "How do I start?"),
                Message::new(MessageRole::Assistant, answer),
            ],
            "What was the command?",
        );
        assert_eq!(follow_up, "run demo-check");
        first_server.join().unwrap();
        second_server.join().unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    pub(crate) fn mock_service(name: &str, model: &str) -> (TcpListener, ModelService) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let service = ModelService {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            enabled: true,
            models: vec![waku_protocol::settings::ServiceModel::new(model.into())],
            legacy_key: false,
        };
        (listener, service)
    }

    pub(crate) fn request(
        listener: &TcpListener,
    ) -> (
        std::net::TcpStream,
        String,
        serde_json::Value,
        Option<String>,
    ) {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut socket);
        let mut method = String::new();
        reader.read_line(&mut method).unwrap();
        let method = method.rsplit_once(' ').unwrap().0.to_owned();
        let mut content_length = 0;
        let mut auth = None;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap();
                } else if name.eq_ignore_ascii_case("authorization") {
                    auth = Some(value.trim().to_owned());
                }
            }
        }
        let mut bytes = vec![0; content_length];
        reader.read_exact(&mut bytes).unwrap();
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (socket, method, body, auth)
    }

    fn send(mut socket: std::net::TcpStream, mime: &str, body: &str) {
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        ).unwrap();
    }

    pub(crate) fn stream(socket: std::net::TcpStream, delta: serde_json::Value, finish: &str) {
        send(
            socket,
            "text/event-stream",
            &format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
            ),
        );
    }

    fn prompt(
        profile: AssistantProfile,
        settings: Arc<DaemonSettingsStore>,
        project: &Path,
        history: Vec<Message>,
        prompt: &str,
    ) -> String {
        let (events, receiver) = super::super::test_event_channel();
        let driver = start(
            profile,
            settings,
            AttachmentStore::new(project.join("attachments")),
            history,
            events,
        )
        .unwrap();
        driver.prompt(prompt.into());
        let mut output = String::new();
        loop {
            match receiver.recv_timeout(Duration::from_secs(10)).unwrap() {
                DriverEvent::TextDelta(text) => output.push_str(&text),
                DriverEvent::TurnFinished { success, summary } => {
                    assert!(success, "{summary:?}");
                    return output;
                }
                DriverEvent::ProcessExited => panic!("Assistant worker stopped early"),
                _ => {}
            }
        }
    }
}
