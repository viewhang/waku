use std::collections::{BTreeMap, HashMap, HashSet};
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
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_service_id: Option<Uuid>,
    /// Endpoint stored by older assistant profiles and sessions
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
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
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ModelPurpose {
    #[default]
    Chat,
    ImageGeneration,
    ImageEdit,
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
        self.purpose == ModelPurpose::Chat
            && matches!(self.kind, ModelKind::Text | ModelKind::Image)
    }

    pub fn accepts(&self, modality: ModelModality) -> bool {
        match self.purpose {
            ModelPurpose::ImageEdit => modality == ModelModality::Vision,
            ModelPurpose::ImageGeneration => false,
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
    /// Add newly discovered models while preserving existing edits, aliases and manual entries
    pub fn merge_catalog(existing: &mut Vec<Self>, incoming: Vec<String>, protocol: ModelProtocol) {
        let known: HashSet<_> = existing
            .iter()
            .flat_map(|model| {
                std::iter::once(model.id.as_str()).chain(model.aliases.iter().map(String::as_str))
            })
            .collect();
        let new: Vec<_> = incoming
            .into_iter()
            .filter(|id| !known.contains(id.as_str()))
            .map(|id| {
                let mut model = Self::new(id);
                model.options.protocol = protocol;
                model
            })
            .collect();
        existing.extend(new);
        existing.sort_by(|a, b| a.options.group.cmp(&b.options.group).then(a.id.cmp(&b.id)));
    }

    pub fn new(id: String) -> Self {
        Self {
            id,
            name: String::new(),
            aliases: Vec::new(),
            options: ServiceModelOptions::default(),
        }
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
            StoredModel::Id(id) => ServiceModel::new(id),
            StoredModel::Model(model) => model,
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
}
