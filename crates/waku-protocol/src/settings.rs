use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

use crate::computer_use::ComputerAppGrant;
use crate::model::ProviderKind;

/// Reusable direct-model configuration saved by the daemon without credentials
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct AssistantProfile {
    pub id: Uuid,
    pub name: String,
    pub system_prompt: String,
    /// Personal library tools can be disabled per assistant, including during an existing chat
    #[serde(default = "knowledge_enabled_by_default")]
    pub knowledge_tools: bool,
    pub model: String,
    /// Previous assistant-wide preference for sessions without a saved choice
    #[serde(default)]
    pub web_search: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_service_id: Option<Uuid>,
    /// Endpoint stored by older assistant profiles and sessions
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
}

fn knowledge_enabled_by_default() -> bool {
    true
}

impl AssistantProfile {
    /// Resolve the session's provider consistently across the desktop and daemon, including old URL-based sessions
    pub fn service<'a>(
        &self,
        services: &'a [ModelService],
        profiles: &[Self],
    ) -> Option<&'a ModelService> {
        if let Some(id) = self.model_service_id {
            return services.iter().find(|service| service.id == id);
        }
        let base_url = self.base_url.trim().trim_end_matches('/');
        let default = profiles
            .iter()
            .find(|profile| profile.id == self.id)
            .and_then(|profile| profile.model_service_id);
        services
            .iter()
            .filter(|service| {
                service.base_url == base_url && (service.legacy_key || default == Some(service.id))
            })
            .max_by_key(|service| default == Some(service.id))
    }
}

/// Provider endpoint and the models exposed in Waku's picker
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct ModelService {
    pub id: Uuid,
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_service_models")]
    pub models: Vec<ServiceModel>,
    /// Older assistant sessions can resolve this migrated service by endpoint
    #[serde(default)]
    pub legacy_key: bool,
}

impl ModelService {
    pub fn model(&self, id: &str) -> Option<&ServiceModel> {
        self.models.iter().find(|model| model.matches(id))
    }
}

/// Remember old IDs so a catalog refresh and existing sessions follow an edit
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct ServiceModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "ServiceModelOptions::is_default")]
    pub options: ServiceModelOptions,
    /// Catalog vector width is informational and never forces an API override
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_embedding_dimensions: Option<u32>,
    /// True while awaiting metadata, false once configured, absent on older structured rows
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_metadata_pending: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ModelPurpose {
    #[default]
    Chat,
    ImageGeneration,
    ImageEdit,
    /// Catalog capability is known but the provider operation has not been configured
    CatalogOnly,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ModelProtocol {
    #[default]
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    Gemini,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ModelKind {
    #[default]
    Text,
    Image,
    Embedding,
    Rerank,
    Speech,
    Transcription,
    Realtime,
    Video,
    Moderation,
    Other,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ModelModality {
    Vision,
    Audio,
    Video,
}

/// Settings selected per model and resolved on every turn
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(default)]
pub struct ServiceModelOptions {
    pub group: String,
    pub purpose: ModelPurpose,
    pub protocol: ModelProtocol,
    pub kind: ModelKind,
    /// Omit dimensions from embedding requests when using the provider default
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_dimensions: Option<std::num::NonZeroU32>,
    pub reasoning: bool,
    /// Whether the model advertises support for function tools
    pub tools: bool,
    pub inputs: Vec<ModelModality>,
    pub context_window: Option<u32>,
    pub max_input_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

impl Default for ServiceModelOptions {
    fn default() -> Self {
        Self {
            group: String::new(),
            purpose: ModelPurpose::Chat,
            protocol: ModelProtocol::OpenAiChat,
            kind: ModelKind::Text,
            embedding_dimensions: None,
            reasoning: false,
            tools: true,
            inputs: Vec::new(),
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
        }
    }
}

impl ServiceModelOptions {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// These models send text or media through the selected dialogue API
    pub fn dialogue(&self) -> bool {
        // An unclassified custom model can still use the configured dialogue protocol
        self.purpose == ModelPurpose::Chat
            && matches!(
                self.kind,
                ModelKind::Text | ModelKind::Image | ModelKind::Unknown
            )
    }

    /// Whether the assistant implements an operation for the selected model
    pub fn assistant_supported(&self) -> bool {
        self.dialogue()
            || match self.purpose {
                ModelPurpose::ImageGeneration | ModelPurpose::ImageEdit => true,
                ModelPurpose::Chat => matches!(self.kind, ModelKind::Embedding | ModelKind::Rerank),
                ModelPurpose::CatalogOnly => false,
            }
    }

    /// Inputs to the embedding API produce one numeric vector per item
    pub fn embedding(&self) -> bool {
        self.purpose == ModelPurpose::Chat && self.kind == ModelKind::Embedding
    }

    pub fn accepts(&self, modality: ModelModality) -> bool {
        match self.purpose {
            ModelPurpose::ImageEdit => modality == ModelModality::Vision,
            ModelPurpose::ImageGeneration | ModelPurpose::CatalogOnly => false,
            ModelPurpose::Chat if self.embedding() => {
                modality == ModelModality::Vision && self.inputs.contains(&modality)
            }
            ModelPurpose::Chat => {
                self.dialogue()
                    && self.inputs.contains(&modality)
                    && self.protocol.supports(modality)
            }
        }
    }

    /// Validate limits for both desktop callers and daemon RPC clients
    pub fn valid_token_limits(&self) -> bool {
        let [window, input, output] = [
            self.context_window,
            self.max_input_tokens,
            self.max_output_tokens,
        ];
        [window, input, output]
            .into_iter()
            .flatten()
            .all(|limit| limit > 0)
            && window.is_none_or(|window| {
                [input, output]
                    .into_iter()
                    .flatten()
                    .all(|limit| limit <= window)
            })
    }
}

impl ModelProtocol {
    pub fn from_host(host: &str) -> Option<Self> {
        match host {
            "api.anthropic.com" => Some(Self::AnthropicMessages),
            "generativelanguage.googleapis.com" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn supports(self, modality: ModelModality) -> bool {
        match modality {
            ModelModality::Vision => true,
            ModelModality::Audio => matches!(self, Self::OpenAiChat | Self::Gemini),
            ModelModality::Video => self == Self::Gemini,
        }
    }
}

impl ServiceModel {
    /// Add discoveries and repair untouched legacy rows without replacing edited models
    pub fn merge_catalog(existing: &mut Vec<Self>, incoming: Vec<Self>) {
        let mut incoming: HashMap<_, _> = incoming
            .into_iter()
            .map(|model| (model.id.clone(), model))
            .collect();
        for model in existing.iter_mut() {
            let discovered = incoming.remove(&model.id);
            for alias in &model.aliases {
                incoming.remove(alias);
            }
            let Some(discovered) = discovered else {
                continue;
            };
            if discovered.catalog_embedding_dimensions.is_some() {
                model.catalog_embedding_dimensions = discovered.catalog_embedding_dimensions;
            }
            if model.catalog_metadata_pending == Some(true)
                || model.catalog_metadata_pending.is_none()
                    && model.matches_legacy_discovery(&discovered)
            {
                model.options = ServiceModelOptions {
                    protocol: model.options.protocol,
                    ..discovered.options
                };
                model.catalog_metadata_pending = discovered.catalog_metadata_pending;
            }
        }
        existing.extend(incoming.into_values());
        existing.sort_by(|a, b| a.options.group.cmp(&b.options.group).then(a.id.cmp(&b.id)));
    }

    pub fn new(id: String) -> Self {
        Self {
            id,
            name: String::new(),
            aliases: Vec::new(),
            options: ServiceModelOptions::default(),
            catalog_embedding_dimensions: None,
            catalog_metadata_pending: Some(false),
        }
    }

    /// A supplier-listed ID can receive public metadata on this or a later sync
    pub fn discovered(id: String, protocol: ModelProtocol) -> Self {
        let mut model = Self::new(id);
        model.options.protocol = protocol;
        // Keep unfamiliar IDs unclassified and avoid advertising unverified tool support
        model.options.kind = ModelKind::Unknown;
        model.options.tools = false;
        model.catalog_metadata_pending = Some(true);
        model
    }

    fn matches_legacy_discovery(&self, discovered: &Self) -> bool {
        if !self.name.is_empty() || !self.aliases.is_empty() {
            return false;
        }
        let protocol = self.options.protocol;
        // Older discovery either stored just defaults or enriched a default text model
        let defaults = ServiceModelOptions {
            protocol,
            ..Default::default()
        };
        let previous_catalog = ServiceModelOptions {
            protocol,
            kind: ModelKind::Text,
            purpose: ModelPurpose::Chat,
            // Old embedding metadata put the catalog vector width in the token output field
            max_output_tokens: discovered.options.max_output_tokens.or(discovered
                .catalog_embedding_dimensions
                .filter(|width| {
                    discovered
                        .options
                        .context_window
                        .is_none_or(|window| *width <= window)
                })),
            ..discovered.options.clone()
        };
        self.options == defaults || self.options == previous_catalog
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    pub fn matches(&self, id: &str) -> bool {
        self.id == id || self.aliases.iter().any(|alias| alias == id)
    }
}

fn deserialize_service_models<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ServiceModel>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredModel {
        Id(String),
        Model(ServiceModel),
    }
    Ok(Vec::<StoredModel>::deserialize(deserializer)?
        .into_iter()
        .map(|model| match model {
            StoredModel::Id(id) => ServiceModel {
                catalog_metadata_pending: Some(true),
                ..ServiceModel::new(id)
            },
            StoredModel::Model(mut model) => {
                // Older catalogs saved embedding width as output tokens, the editor only set it for chat
                if model.options.embedding()
                    && let Some(width) = model
                        .options
                        .max_output_tokens
                        .take()
                        .filter(|width| *width > 1)
                {
                    model.catalog_embedding_dimensions.get_or_insert(width);
                }
                model
            }
        })
        .collect())
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(default)]
pub struct DaemonSettings {
    pub computer_use_enabled: bool,
    pub computer_use_allowed_apps: Vec<ComputerAppGrant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assistants: Vec<AssistantProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_services: Vec<ModelService>,
    pub disabled_providers: Vec<ProviderKind>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub provider_binary_overrides: HashMap<ProviderKind, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            computer_use_enabled: false,
            computer_use_allowed_apps: Vec::new(),
            assistants: Vec::new(),
            model_services: Vec::new(),
            disabled_providers: Vec::new(),
            provider_binary_overrides: HashMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl DaemonSettings {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".waku")
            .join("settings.json")
    }

    /// Keep local credentials separate from the settings document and task database
    pub fn model_service_key_db_path(settings_path: &Path) -> PathBuf {
        settings_path
            .with_file_name("credentials")
            .join("model-services.sqlite3")
    }

    pub fn discard_legacy_app_keys(&mut self) {
        for key in ["analytics_enabled", "favorite_models", "theme", "language"] {
            self.extra.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_options_round_trip_with_legacy_catalog_entries() {
        let settings: DaemonSettings = serde_json::from_value(serde_json::json!({
            "model_services": [{
                "id": "b17bd5f5-8e13-429f-9130-a36f90a7b940",
                "name": "Demo",
                "base_url": "https://example.com/v1",
                "enabled": true,
                "models": ["legacy", {"id": "custom", "options": {
                    "group": "Research", "purpose": "chat", "protocol": "gemini", "kind": "text",
                    "reasoning": true, "tools": false, "inputs": ["vision", "audio", "video"],
                    "context_window": 128000, "max_input_tokens": 100000, "max_output_tokens": 8192
                }}]
            }]
        }))
        .unwrap();
        let models = &settings.model_services[0].models;
        assert_eq!(models[0].options, ServiceModelOptions::default());
        assert_eq!(models[1].options.group, "Research");
        assert!(models[1].options.valid_token_limits());
        assert!(models[1].options.accepts(ModelModality::Video));
        let round_trip: DaemonSettings =
            serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
        assert_eq!(round_trip, settings);
    }

    #[test]
    fn embedding_dimensions_round_trip_independently_of_token_limits() {
        let legacy: ModelService = serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(), "name": "Local", "base_url": "http://localhost/v1", "enabled": true,
            "models": [{"id": "encoder", "options": {"kind": "embedding", "context_window": 8192, "max_output_tokens": 1536}}]
        })).unwrap();
        assert_eq!(legacy.models[0].catalog_embedding_dimensions, Some(1536));
        assert_eq!(legacy.models[0].options.embedding_dimensions, None);
        assert_eq!(legacy.models[0].options.max_output_tokens, None);
        let mut data = serde_json::json!({"kind": "embedding", "max_input_tokens": 8, "embedding_dimensions": 512});
        let options: ServiceModelOptions = serde_json::from_value(data.clone()).unwrap();
        assert!(options.embedding() && !options.dialogue() && options.valid_token_limits());
        assert_eq!(
            serde_json::to_value(options).unwrap()["embedding_dimensions"],
            512
        );
        data["embedding_dimensions"] = 0.into();
        assert!(serde_json::from_value::<ServiceModelOptions>(data).is_err());
    }

    #[test]
    fn catalog_fills_pending_models_without_overwriting_saved_choices() {
        let pending = |id: &str| ServiceModel::discovered(id.into(), ModelProtocol::Gemini);
        let mut edited = ServiceModel::new("edited".into());
        edited.name = "My model".into();
        edited.aliases.push("old-id".into());
        edited.options.tools = false;
        edited.options.max_output_tokens = Some(1024);
        let mut models = vec![edited.clone(), pending("pending")];
        let incoming = ["pending", "edited", "old-id", "new"]
            .map(|id| {
                let mut model = ServiceModel::new(id.into());
                model.options.tools = true;
                model.options.max_output_tokens = Some(8192);
                model
            })
            .into();

        ServiceModel::merge_catalog(&mut models, incoming);

        assert_eq!(models.len(), 3);
        assert_eq!(models[0], edited);
        assert_eq!(models[1].id, "new");
        assert_eq!(models[2].options.protocol, ModelProtocol::Gemini);
        assert!(models[2].options.tools);
        assert_eq!(models[2].options.max_output_tokens, Some(8192));
        assert_eq!(models[2].catalog_metadata_pending, Some(false));
    }

    #[test]
    fn catalog_repairs_legacy_defaults_and_preserves_explicit_model_settings() {
        let legacy: ServiceModel =
            serde_json::from_value(serde_json::json!({"id": "tts"})).unwrap();
        let locked = ServiceModel::new("manual".into());
        let mut old_embedding = ServiceModel::new("embedding".into());
        old_embedding.catalog_metadata_pending = None;
        old_embedding.options.tools = false;
        old_embedding.options.context_window = Some(8191);
        let mut models = vec![legacy, locked.clone(), old_embedding.clone()];
        let mut tts = ServiceModel::discovered("tts".into(), ModelProtocol::OpenAiChat);
        tts.options.kind = ModelKind::Speech;
        let mut embedding = old_embedding;
        embedding.options.kind = ModelKind::Embedding;
        embedding.catalog_metadata_pending = Some(false);
        let mut attempted_override = tts.clone();
        attempted_override.id = "manual".into();
        ServiceModel::merge_catalog(&mut models, vec![tts, embedding, attempted_override]);

        assert_eq!(models[0].options.kind, ModelKind::Embedding);
        assert_eq!(models[0].options.context_window, Some(8191));
        assert_eq!(models[1], locked);
        assert_eq!(models[2].options.kind, ModelKind::Speech);
        assert!(!models[2].options.tools);
        assert!(!models[2].options.assistant_supported());
    }
}
