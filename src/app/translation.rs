//! Standalone text translation using the configured model services

use super::assistants::{ModelSelection, assistant_button_base};
use super::*;
use crate::ui::ActivationExt as _;
use waku_client::{Command, ResponsePayload};
use waku_protocol::TranslationLanguage;

const PAGE_CONTEXT: &str = "TranslationPage";
gpui::actions!(translation, [CloseTranslation]);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new(
        "escape",
        CloseTranslation,
        Some(PAGE_CONTEXT),
    )]);
}

pub(super) struct TranslationPage {
    open: bool,
    source: Entity<TextInput>,
    output: Entity<TextInput>,
    source_language: Option<TranslationLanguage>,
    target_language: TranslationLanguage,
    pub(super) model: Option<ModelSelection>,
    pub(super) busy: bool,
    error: Option<String>,
}

impl TranslationPage {
    /// Keep the input and result while navigating away during this app run
    fn new(
        window: &mut Window,
        language: &str,
        model: Option<ModelSelection>,
        cx: &mut App,
    ) -> Self {
        let target_language = match language {
            "zh-CN" => TranslationLanguage::SimplifiedChinese,
            "ja" => TranslationLanguage::Japanese,
            _ => TranslationLanguage::English,
        };
        Self {
            open: false,
            source: cx.new(|cx| {
                TextInput::new(window, cx)
                    .multi_line()
                    .tab_stop()
                    .placeholder(tr!("translation.input"))
            }),
            output: cx.new(|cx| {
                TextInput::new(window, cx)
                    .multi_line()
                    .tab_stop()
                    .read_only(true)
            }),
            source_language: None,
            target_language,
            model,
            busy: false,
            error: None,
        }
    }

    pub(super) fn clear_result(&mut self, cx: &mut App) {
        self.output.update(cx, |input, cx| input.clear(cx));
        self.error = None;
    }
}

impl Waku {
    pub(super) fn translation_is_open(&self) -> bool {
        self.translation.as_ref().is_some_and(|page| page.open)
    }

    pub(super) fn translation_source_focus(&self, cx: &App) -> Option<FocusHandle> {
        self.translation
            .as_ref()
            .filter(|page| page.open)
            .map(|page| page.source.read(cx).focus_handle(cx))
    }

    pub(super) fn open_translation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.translation.is_none() {
            let default_model = self
                .state
                .model_services
                .iter()
                .filter(|service| service.enabled)
                .find_map(|service| {
                    let model = service
                        .models
                        .iter()
                        .find(|model| model.options.dialogue())?;
                    Some(ModelSelection {
                        service_id: service.id,
                        model: model.id.clone(),
                    })
                });
            let page =
                TranslationPage::new(window, self.state.language.locale(), default_model, cx);
            cx.subscribe(&page.source, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Edited)
                    && let Some(page) = this.translation.as_mut()
                    && !page.busy
                {
                    page.clear_result(cx);
                    cx.notify();
                }
            })
            .detach();
            self.translation = Some(page);
        }
        let page = self
            .translation
            .as_mut()
            .expect("translation page was created above");
        page.open = true;
        let focus = page.source.read(cx).focus_handle(cx);
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn leave_translation(&mut self) {
        if let Some(page) = self.translation.as_mut() {
            page.open = false;
        }
    }

    pub(super) fn close_translation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_translation();
        window.focus(&self.composer_focus(cx), cx);
        cx.notify();
    }

    fn translation_model_available(&self, selection: &ModelSelection) -> bool {
        self.model_service(selection.service_id)
            .filter(|service| service.enabled)
            .and_then(|service| service.model(&selection.model))
            .is_some_and(|model| model.options.dialogue())
    }

    fn translate(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.translation.as_ref().filter(|page| !page.busy) else {
            return;
        };
        let Some(model) = page
            .model
            .as_ref()
            .filter(|model| self.translation_model_available(model))
        else {
            return;
        };
        let text = page.source.read(cx).content();
        if text.trim().is_empty() || page.source_language == Some(page.target_language) {
            return;
        }
        let command = Command::TranslateText {
            service_id: model.service_id,
            model: model.model.clone(),
            text: text.to_owned(),
            source_language: page.source_language,
            target_language: page.target_language,
        };
        let page = self
            .translation
            .as_mut()
            .expect("translation page remains present");
        page.busy = true;
        page.clear_result(cx);
        page.source.update(cx, |input, cx| {
            input.set_read_only(true);
            cx.notify();
        });
        let client = self.daemon.client();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // Give the model call its own mailbox so settings requests can run concurrently
                    client.request(Uuid::new_v4(), Uuid::nil(), command)
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                if let Some(page) = this.translation.as_mut() {
                    page.busy = false;
                    page.source.update(cx, |input, cx| {
                        input.set_read_only(false);
                        cx.notify();
                    });
                    match result {
                        Ok(ResponsePayload::Translation { text }) => page
                            .output
                            .update(cx, |input, cx| input.set_content(text, cx)),
                        Ok(_) => page.error = Some(tr!("assistant.invalid_response")),
                        Err(error) => page.error = Some(tr!("translation.failed", error = error)),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn swap_translation_languages(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.translation.as_mut().filter(|page| !page.busy) else {
            return;
        };
        let Some(source) = page.source_language else {
            return;
        };
        page.source_language = Some(page.target_language);
        page.target_language = source;
        let result = page.output.read(cx).content().to_owned();
        if !result.is_empty() {
            page.source
                .update(cx, |input, cx| input.set_content(result, cx));
        }
        page.clear_result(cx);
        cx.notify();
    }

    fn translation_language_menu(
        &self,
        page: &TranslationPage,
        source: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (id, selected) = if source {
            ("translation-source-language", page.source_language)
        } else {
            ("translation-target-language", Some(page.target_language))
        };
        let label = selected.map_or_else(
            || tr!("translation.auto"),
            |language| language.names().0.to_owned(),
        );
        let handle = self.menu_handle(id, cx);
        let trigger = MenuChip::new(id)
            .label(label)
            .selected(handle.is_open())
            .disabled(page.busy);
        if page.busy {
            return trigger.into_any_element();
        }
        let weak = cx.weak_entity();
        dropdown_menu(trigger, id, &handle, MenuAlign::BelowLeft, move |_| {
            let choose = |language: Option<TranslationLanguage>, label: String| {
                let weak = weak.clone();
                MenuItem::new(label, move |_, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        let Some(page) = this.translation.as_mut().filter(|page| !page.busy) else {
                            return;
                        };
                        if source {
                            page.source_language = language;
                        } else if let Some(language) = language {
                            page.target_language = language;
                        }
                        page.clear_result(cx);
                        cx.notify();
                    });
                })
                .selected(selected == language)
            };
            source
                .then(|| choose(None, tr!("translation.auto")))
                .into_iter()
                .chain(
                    TranslationLanguage::ALL
                        .into_iter()
                        .map(|(language, label, _)| choose(Some(language), label.to_owned())),
                )
                .collect()
        })
    }

    pub(super) fn render_translation(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let page = self.translation.as_ref().expect("translation is open");
        let theme = Theme::current(cx);
        let same_language = page.source_language == Some(page.target_language);
        let can_translate = !page.busy
            && !same_language
            && !page.source.read(cx).content().trim().is_empty()
            && page
                .model
                .as_ref()
                .is_some_and(|model| self.translation_model_available(model));
        let can_copy = !page.output.read(cx).content().is_empty();
        let source = self.translation_language_menu(page, true, cx);
        let target = self.translation_language_menu(page, false, cx);
        let model = self.render_translation_model_picker(page.model.as_ref(), page.busy, cx);
        let stacked = window.viewport_size().width - px(self.sidebar_rendered_width) < px(580.0);

        let back = self.translation_button(
            "translation-back",
            tr!("translation.back"),
            true,
            cx,
            Self::close_translation,
        );
        let copy = self.translation_button(
            "translation-copy",
            tr!("translation.copy"),
            can_copy,
            cx,
            |this, _, cx| {
                if let Some(page) = this.translation.as_ref() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        page.output.read(cx).content().to_owned(),
                    ));
                    this.show_success_toast(tr!("translation.copied"));
                    cx.notify();
                }
            },
        );
        let swap = self
            .translation_button(
                "translation-swap",
                "⇄".into(),
                !page.busy && !same_language && page.source_language.is_some(),
                cx,
                |this, _, cx| {
                    this.swap_translation_languages(cx);
                },
            )
            .tooltip(Tooltip::text(tr!("translation.swap")));
        let translate = self
            .translation_button(
                "translation-submit",
                if page.busy {
                    tr!("translation.translating")
                } else {
                    tr!("translation.title")
                },
                can_translate,
                cx,
                |this, _, cx| this.translate(cx),
            )
            .when(can_translate, |button| {
                button.bg(theme.inverse).text_color(theme.on_inverse)
            });

        div()
            .id("translation-page")
            .key_context(PAGE_CONTEXT)
            .on_action(cx.listener(|this, _: &CloseTranslation, window, cx| {
                this.close_translation(window, cx);
                cx.stop_propagation();
            }))
            .tab_group()
            .tab_stop(false)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(20.0))
            .when(!self.sidebar_visible, |page| page.pt(px(38.0)))
            .child(
                div().flex().items_center().gap(px(12.0)).child(back).child(
                    div()
                        .text_size(sp(18.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tr!("translation.title")),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .child(source)
                    .child(swap)
                    .child(target)
                    .child(div().flex_1())
                    .child(model)
                    .child(translate),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(12.0))
                    .when(stacked, |panes| panes.flex_col())
                    .child(translation_text_pane(
                        "translation-source-pane",
                        tr!("translation.source"),
                        page.source.clone(),
                        None,
                        theme,
                        cx,
                    ))
                    .child(translation_text_pane(
                        "translation-output-pane",
                        tr!("translation.output"),
                        page.output.clone(),
                        Some(copy),
                        theme,
                        cx,
                    )),
            )
            .children(
                page.error
                    .clone()
                    .or_else(|| same_language.then(|| tr!("translation.same_language")))
                    .map(|error| {
                        div()
                            .text_size(sp(12.0))
                            .text_color(theme.warning)
                            .child(error)
                    }),
            )
    }

    fn translation_button(
        &self,
        id: &'static str,
        label: String,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        assistant_button_base(id, label, theme)
            .tab_stop(enabled)
            .when(!enabled, |button| button.text_color(theme.text_tertiary))
            .when(enabled, |button| {
                button
                    .hover(|style| style.opacity(0.85))
                    .on_activation(cx, action)
            })
    }
}

fn translation_text_pane(
    id: &'static str,
    label: String,
    input: Entity<TextInput>,
    button: Option<Stateful<Div>>,
    theme: Theme,
    cx: &App,
) -> Div {
    let focus = input.read(cx).focus_handle(cx);
    div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .h(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .text_size(sp(12.0))
                .text_color(theme.text_secondary)
                .child(label)
                .children(button),
        )
        .child(
            div()
                .id(id)
                .tab_index(0)
                .tab_stop(false)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border_strong)
                .in_focus(|style| style.border_color(theme.accent))
                .bg(theme.composer)
                .p(px(10.0))
                .text_size(sp(14.0))
                .line_height(sp(22.0))
                .child(input)
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    window.focus(&focus, cx);
                }),
        )
}
