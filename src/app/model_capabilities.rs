//! Model list categories and badges derived from the saved capability settings

use super::*;
use waku_protocol::settings::{ModelKind, ModelModality, ModelPurpose, ServiceModelOptions};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ModelCategory {
    #[default]
    All,
    Text,
    Image,
    Embedding,
    Audio,
    Video,
    Rerank,
}

impl ModelCategory {
    pub(super) const ALL: [Self; 7] = [
        Self::All,
        Self::Text,
        Self::Image,
        Self::Embedding,
        Self::Audio,
        Self::Video,
        Self::Rerank,
    ];

    pub(super) fn index(self) -> usize {
        self as usize
    }

    pub(super) fn matches(self, options: &ServiceModelOptions) -> bool {
        match self {
            Self::All => true,
            Self::Text => options.purpose == ModelPurpose::Chat && options.kind == ModelKind::Text,
            // Image generation and editing are distinct from a text model accepting an image
            Self::Image => {
                options.kind == ModelKind::Image || options.purpose != ModelPurpose::Chat
            }
            Self::Embedding => {
                options.purpose == ModelPurpose::Chat && options.kind == ModelKind::Embedding
            }
            Self::Audio => options.accepts(ModelModality::Audio),
            Self::Video => options.accepts(ModelModality::Video),
            Self::Rerank => {
                options.purpose == ModelPurpose::Chat && options.kind == ModelKind::Rerank
            }
        }
    }

    pub(super) fn label(self) -> String {
        match self {
            Self::All => tr!("service_model.all"),
            Self::Text => tr!("service_model.text"),
            Self::Image => tr!("service_model.image"),
            Self::Embedding => tr!("service_model.embedding"),
            Self::Audio => tr!("service_model.audio"),
            Self::Video => tr!("service_model.video"),
            Self::Rerank => tr!("service_model.rerank"),
        }
    }

    pub(super) fn icon(self) -> &'static str {
        match self {
            Self::All => "icons/list.svg",
            Self::Text => "icons/case-sensitive.svg",
            Self::Image => "icons/file-types/image.svg",
            Self::Embedding => "icons/package.svg",
            Self::Audio => "icons/file-types/audio.svg",
            Self::Video => "icons/file-types/video.svg",
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
            ModelCategory::Audio.icon(),
            theme.gauge,
        ),
        (
            options.accepts(ModelModality::Video),
            "service_model.video_input",
            ModelCategory::Video.icon(),
            theme.gauge,
        ),
        (
            options.purpose == ModelPurpose::ImageGeneration,
            "service_model.generate",
            ModelCategory::Image.icon(),
            theme.success,
        ),
        (
            options.purpose == ModelPurpose::ImageEdit,
            "service_model.edit_image",
            "icons/pencil.svg",
            theme.success,
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
