//! Optional Models.dev metadata for supplier-listed assistant models

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use url::{Host, Url};
use waku_protocol::settings::{ModelKind, ModelModality, ModelPurpose, ServiceModel};

use super::{http_client, service_url};

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Deserialize)]
struct Provider {
    api: Option<String>,
    models: HashMap<String, Value>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "type")]
    kind: Option<String>,
    reasoning: bool,
    tool_call: bool,
    modalities: Modalities,
    limit: Limits,
}

#[derive(Deserialize)]
struct Modalities {
    input: Vec<String>,
}

#[derive(Deserialize)]
struct Limits {
    context: Option<u32>,
    input: Option<u32>,
    output: Option<u32>,
}

/// Skip HTTP, IP and local-name services before contacting the public catalog
pub(super) fn enrich(endpoint: &Url, models: &mut [ServiceModel]) {
    if models.is_empty()
        || endpoint.scheme() != "https"
        || !matches!(endpoint.host(), Some(Host::Domain(host)) if host.contains('.') && !host.ends_with(".local") && !host.ends_with(".localhost"))
    {
        return;
    }
    if provider_id(endpoint) == Some("openai") {
        apply_openai_reference(models);
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let catalog = runtime.block_on(fetch_catalog(CATALOG_URL, CATALOG_TIMEOUT));
    // A blocking system DNS lookup can outlive the HTTP deadline
    runtime.shutdown_background();
    if let Some(catalog) = catalog {
        apply_catalog(endpoint, &catalog, models);
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;
    use crate::driver::assistant::tests::{request, send};
    use waku_protocol::settings::ModelProtocol;

    #[test]
    fn openai_catalog_classifies_the_reported_ids_and_preserves_unknowns() {
        let catalog = serde_json::from_value(serde_json::json!({
            "openai": {"models": {"text-embedding-3-small": {
                "type": "embedding", "reasoning": false, "tool_call": false,
                "modalities": {"input": ["text"], "output": ["text"]},
                "limit": {"context": 8191, "output": 1536}
            }}},
            "other": {"api": "https://example.com/v1", "models": {"video": {
                "type": "video", "reasoning": false, "tool_call": false,
                "modalities": {"input": ["text"], "output": ["video"]},
                "limit": {"context": 0, "output": 0}
            }}}
        }))
        .unwrap();
        let discovered = |id: &str| ServiceModel::discovered(id.into(), ModelProtocol::OpenAiChat);
        let mut models = [
            "tts-1-hd-1106",
            "whisper-1",
            "gpt-realtime-2.1",
            "text-embedding-3-small",
            "omni-moderation-latest",
            "o3-2025-04-16",
            "tts-1-unknown",
        ]
        .map(discovered);
        apply_openai_reference(&mut models);
        apply_catalog(
            &service_url("https://api.openai.com/v1").unwrap(),
            &catalog,
            &mut models,
        );

        assert_eq!(
            models.each_ref().map(|model| model.options.kind),
            [
                ModelKind::Speech,
                ModelKind::Transcription,
                ModelKind::Realtime,
                ModelKind::Embedding,
                ModelKind::Moderation,
                ModelKind::Text,
                ModelKind::Unknown,
            ]
        );
        assert!(
            models[..3]
                .iter()
                .all(|model| !model.options.tools && !model.options.assistant_supported())
        );
        assert_eq!(models[3].options.context_window, Some(8191));
        assert!(models[3].options.assistant_supported());
        assert!(models[5].options.reasoning && models[5].options.tools);
        assert_eq!(models[6], discovered("tts-1-unknown"));

        let mut other = [discovered("tts-1-hd-1106"), discovered("video")];
        apply_catalog(
            &service_url("https://example.com/v1").unwrap(),
            &catalog,
            &mut other,
        );
        assert_eq!(other[0], discovered("tts-1-hd-1106"));
        assert_eq!(other[1].options.kind, ModelKind::Video);
    }

    #[tokio::test]
    async fn catalog_matches_exact_supplier_and_model_and_times_out_waiting_for_body() {
        let catalog = r#"{
            "openai": {"models": {"exact": {
                "reasoning": true, "tool_call": true,
                "modalities": {"input": ["text", "image", "audio", "video", "pdf"]},
                "limit": {"context": 128000, "input": 100000, "output": 8192}
            }}},
            "other": {"api": "https://example.com/v1", "models": {"different": {
                "reasoning": true, "tool_call": true,
                "modalities": {"input": ["text"]},
                "limit": {"context": 1000, "output": 500}
            }}}
        }"#;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = format!("http://{}/api.json", listener.local_addr().unwrap());
        let (finished, waiting) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            use std::io::Write as _;

            let (socket, method, _, auth) = request(&listener);
            assert_eq!(method, "GET /api.json");
            assert_eq!(auth, None);
            send(socket, "application/json", catalog);

            let (mut socket, _, _, _) = request(&listener);
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{");
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        });

        let catalog = fetch_catalog(&address, Duration::from_secs(3))
            .await
            .unwrap();
        let unresolved = |id: &str| ServiceModel::discovered(id.into(), ModelProtocol::OpenAiChat);
        let mut models = [
            unresolved("exact"),
            unresolved("exact-alias"),
            unresolved("different"),
        ];
        apply_catalog(
            &service_url("https://api.openai.com/v1/").unwrap(),
            &catalog,
            &mut models,
        );
        let options = &models[0].options;
        assert!(options.reasoning && options.tools);
        assert_eq!(
            options.inputs,
            [
                ModelModality::Vision,
                ModelModality::Audio,
                ModelModality::Video
            ]
        );
        assert_eq!(options.context_window, Some(128000));
        assert_eq!(options.max_input_tokens, Some(100000));
        assert_eq!(options.max_output_tokens, Some(8192));
        assert_eq!(models[1], unresolved("exact-alias"));
        assert_eq!(models[2], unresolved("different"));

        let endpoint = service_url("https://example.com/v1").unwrap();
        apply_catalog(&endpoint, &catalog, &mut models[2..]);
        assert_eq!(models[2].options.max_output_tokens, Some(500));
        assert_eq!(models[2].options.max_input_tokens, None);

        assert!(
            fetch_catalog(&address, Duration::from_millis(100))
                .await
                .is_none()
        );
        finished.send(()).unwrap();
        server.join().unwrap();
    }
}

/// Download only the public catalog, without supplier URLs, credentials or model IDs
async fn fetch_catalog(address: &str, timeout: Duration) -> Option<HashMap<String, Provider>> {
    let response = http_client()
        .ok()?
        .get(address)
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Some native SDK providers omit their well-known API roots in Models.dev
fn provider_id(endpoint: &Url) -> Option<&'static str> {
    match endpoint.as_str().trim_end_matches('/') {
        "https://api.openai.com/v1" => Some("openai"),
        "https://api.anthropic.com" | "https://api.anthropic.com/v1" => Some("anthropic"),
        "https://generativelanguage.googleapis.com"
        | "https://generativelanguage.googleapis.com/v1"
        | "https://generativelanguage.googleapis.com/v1beta" => Some("google"),
        "https://openrouter.ai/api/v1" => Some("openrouter"),
        "https://api.deepseek.com/v1" => Some("deepseek"),
        "https://api.groq.com/openai/v1" => Some("groq"),
        "https://api.mistral.ai/v1" => Some("mistral"),
        "https://api.x.ai/v1" => Some("xai"),
        _ => None,
    }
}

fn apply_catalog(endpoint: &Url, catalog: &HashMap<String, Provider>, models: &mut [ServiceModel]) {
    let provider_key = provider_id(endpoint);
    let provider = if let Some(id) = provider_key {
        catalog.get(id)
    } else {
        let mut matches = catalog.values().filter(|provider| {
            provider
                .api
                .as_deref()
                .and_then(|api| service_url(api).ok())
                .as_ref()
                == Some(endpoint)
        });
        let first = matches.next();
        // An address shared by several catalog providers cannot identify one of them
        first.filter(|_| matches.next().is_none())
    };
    let Some(provider) = provider else { return };
    for model in models {
        let Some(metadata) = provider
            .models
            .get(&model.id)
            .and_then(|entry| Metadata::deserialize(entry).ok())
        else {
            continue;
        };
        let options = &mut model.options;
        // Keep exact official types when the older catalog omits their type field
        if options.kind == ModelKind::Unknown {
            options.kind = match metadata.kind.as_deref() {
                None | Some("chat") => ModelKind::Text,
                Some("image") => ModelKind::Image,
                Some("embedding") => ModelKind::Embedding,
                Some("reranking") => ModelKind::Rerank,
                Some("speech") => ModelKind::Speech,
                Some("transcription") => ModelKind::Transcription,
                Some("realtime") => ModelKind::Realtime,
                Some("video") => ModelKind::Video,
                Some("moderation") => ModelKind::Moderation,
                Some(_) => ModelKind::Other,
            };
        }
        if options.kind == ModelKind::Image {
            // A catalog type alone does not identify another provider's image endpoint
            options.purpose = if provider_key == Some("openai") {
                ModelPurpose::ImageGeneration
            } else {
                ModelPurpose::CatalogOnly
            };
        }
        options.reasoning = metadata.reasoning;
        options.tools = metadata.tool_call;
        options.inputs = [
            ("image", ModelModality::Vision),
            ("audio", ModelModality::Audio),
            ("video", ModelModality::Video),
        ]
        .into_iter()
        .filter(|(name, _)| metadata.modalities.input.iter().any(|input| input == name))
        .map(|(_, modality)| modality)
        .collect();
        options.context_window = metadata.limit.context.filter(|&limit| limit > 0);
        let valid_limit = |&limit: &u32| {
            limit > 0
                && options
                    .context_window
                    .is_none_or(|context| limit <= context)
        };
        options.max_input_tokens = metadata.limit.input.filter(valid_limit);
        options.max_output_tokens = metadata.limit.output.filter(valid_limit);
        model.catalog_metadata_pending = Some(false);
    }
}

/// Add exact IDs documented by OpenAI but omitted from the public catalog
fn apply_openai_reference(models: &mut [ServiceModel]) {
    for model in models {
        let options = &mut model.options;
        options.kind = match model.id.as_str() {
            // https://github.com/openai/openai-cli/blob/main/docs/audio-workbench.md
            "tts-1"
            | "tts-1-1106"
            | "tts-1-hd"
            | "tts-1-hd-1106"
            | "gpt-4o-mini-tts"
            | "gpt-4o-mini-tts-2025-03-20"
            | "gpt-4o-mini-tts-2025-12-15" => ModelKind::Speech,
            // https://developers.openai.com/api/docs/guides/transcription
            "whisper-1"
            | "gpt-transcribe"
            | "gpt-live-transcribe"
            | "gpt-4o-transcribe"
            | "gpt-4o-mini-transcribe"
            | "gpt-4o-transcribe-diarize"
            | "gpt-realtime-whisper" => {
                options.inputs = vec![ModelModality::Audio];
                ModelKind::Transcription
            }
            // https://developers.openai.com/api/reference/resources/realtime/server-events
            "gpt-realtime"
            | "gpt-realtime-2025-08-28"
            | "gpt-realtime-mini"
            | "gpt-realtime-mini-2025-10-06"
            | "gpt-realtime-mini-2025-12-15"
            | "gpt-realtime-2"
            | "gpt-realtime-2.1"
            | "gpt-realtime-2.1-mini"
            | "gpt-realtime-translate" => {
                options.inputs = vec![ModelModality::Audio];
                ModelKind::Realtime
            }
            "omni-moderation-latest" | "omni-moderation-2024-09-26" => ModelKind::Moderation,
            "text-embedding-3-large" | "text-embedding-3-small" | "text-embedding-ada-002" => {
                ModelKind::Embedding
            }
            // https://developers.openai.com/api/docs/models/o3
            "o3-2025-04-16" => {
                options.reasoning = true;
                options.tools = true;
                options.inputs = vec![ModelModality::Vision];
                options.context_window = Some(200_000);
                options.max_output_tokens = Some(100_000);
                ModelKind::Text
            }
            _ => continue,
        };
    }
}
