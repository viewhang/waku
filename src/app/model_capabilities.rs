//! Model list categories and badges derived from the saved capability settings

use super::*;
use waku_protocol::settings::{ModelKind, ModelModality, ModelPurpose, ServiceModelOptions};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ModelCategory {
    #[default]
    All,
    Chat,
    Speech,
    Embedding,
    Rerank,
}

impl ModelCategory {
    pub(super) const TYPES: [Self; 4] = [Self::Chat, Self::Speech, Self::Embedding, Self::Rerank];
    pub(super) const ALL: [Self; 5] = [
        Self::All,
        Self::Chat,
        Self::Speech,
        Self::Embedding,
        Self::Rerank,
    ];

    pub(super) fn index(self) -> usize {
        self as usize
    }

    /// Provider operation subtypes are kept for dispatch while the editor and filters share four types
    pub(super) fn matches(self, options: &ServiceModelOptions) -> bool {
        match self {
            Self::All => true,
            Self::Chat => options.dialogue(),
            Self::Speech => matches!(
                options.kind,
                ModelKind::Speech | ModelKind::Transcription | ModelKind::Realtime
            ),
            Self::Embedding => options.embedding(),
            Self::Rerank => {
                options.purpose == ModelPurpose::Chat && options.kind == ModelKind::Rerank
            }
        }
    }

    pub(super) fn select(self, options: &mut ServiceModelOptions) {
        // Selecting an already classified voice model keeps its synthesis or transcription subtype
        if self.matches(options) {
            return;
        }
        options.kind = match self {
            Self::Chat => ModelKind::Text,
            Self::Speech => ModelKind::Speech,
            Self::Embedding => ModelKind::Embedding,
            Self::Rerank => ModelKind::Rerank,
            Self::All => return,
        };
        options.purpose = ModelPurpose::Chat;
        // Capabilities and token limits apply only to the operation they were configured for
        options.inputs.clear();
        options.reasoning = false;
        options.tools = false;
        options.context_window = None;
        options.max_input_tokens = None;
        options.max_output_tokens = None;
    }

    pub(super) fn label(self) -> String {
        match self {
            Self::All => tr!("service_model.all"),
            Self::Chat => tr!("service_model.chat"),
            Self::Speech => tr!("service_model.voice"),
            Self::Embedding => tr!("service_model.embedding"),
            Self::Rerank => tr!("service_model.rerank"),
        }
    }

    pub(super) fn icon(self) -> &'static str {
        match self {
            Self::All => "icons/list.svg",
            Self::Chat => "icons/case-sensitive.svg",
            Self::Speech => "icons/file-types/audio.svg",
            Self::Embedding => "icons/package.svg",
            Self::Rerank => "icons/chevrons-up-down.svg",
        }
    }
}

pub(super) fn model_capability_badges(options: &ServiceModelOptions, theme: Theme) -> Div {
    let badges = [
        (
            options.dialogue() && options.accepts(ModelModality::Vision),
            "service_model.vision",
            "icons/eye.svg",
            theme.success,
        ),
        (
            options.embedding() && options.accepts(ModelModality::Vision),
            "service_model.image_input",
            "icons/file-types/image.svg",
            theme.success,
        ),
        (
            options.dialogue() && options.reasoning,
            "service_model.reasoning",
            "icons/sparkle.svg",
            theme.gauge,
        ),
        (
            options.dialogue() && options.tools,
            "service_model.tools",
            "icons/wrench.svg",
            theme.accent,
        ),
        (
            options.accepts(ModelModality::Audio),
            "service_model.audio_input",
            "icons/file-types/audio.svg",
            theme.gauge,
        ),
        (
            options.accepts(ModelModality::Video),
            "service_model.video_input",
            "icons/file-types/video.svg",
            theme.gauge,
        ),
        (
            options.purpose == ModelPurpose::ImageGeneration,
            "service_model.generate",
            "icons/file-types/image.svg",
            theme.success,
        ),
        (
            options.purpose == ModelPurpose::ImageEdit,
            "service_model.edit_image",
            "icons/pencil.svg",
            theme.success,
        ),
        (
            options.kind == ModelKind::Image && options.purpose == ModelPurpose::CatalogOnly,
            "service_model.image",
            "icons/file-types/image.svg",
            theme.text_secondary,
        ),
        (
            ModelCategory::Embedding.matches(options),
            "service_model.embedding",
            ModelCategory::Embedding.icon(),
            theme.text_secondary,
        ),
        (
            ModelCategory::Rerank.matches(options),
            "service_model.rerank",
            ModelCategory::Rerank.icon(),
            theme.text_secondary,
        ),
        (
            options.kind == ModelKind::Speech,
            "service_model.speech",
            ModelCategory::Speech.icon(),
            theme.gauge,
        ),
        (
            options.kind == ModelKind::Transcription,
            "service_model.transcription",
            "icons/file.svg",
            theme.gauge,
        ),
        (
            options.kind == ModelKind::Realtime,
            "service_model.realtime",
            "icons/file-types/audio.svg",
            theme.gauge,
        ),
        (
            options.kind == ModelKind::Video,
            "service_model.video",
            "icons/file-types/video.svg",
            theme.gauge,
        ),
        (
            options.kind == ModelKind::Moderation,
            "service_model.moderation",
            "icons/package.svg",
            theme.text_secondary,
        ),
    ];
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .children(
            badges
                .into_iter()
                .filter(|(visible, _, _, _)| *visible)
                .map(|(_, capability, glyph, color)| {
                    let label = tr!(capability);
                    div()
                        .id(capability)
                        .aria_label(label.clone())
                        .tooltip(Tooltip::text(label))
                        .w(px(27.0))
                        .h(px(22.0))
                        .rounded_full()
                        .bg(color.opacity(0.12))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .child(icon(glyph, 13.0, color))
                }),
        )
}
