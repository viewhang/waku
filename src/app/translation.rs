//! Compare a configured model and Google Translate on the same text

use super::assistants::{ModelSelection, assistant_button_base};
use super::*;
use crate::ui::ActivationExt as _;
use waku_client::{Command, ResponsePayload};
use waku_protocol::settings::ModelService;
use waku_protocol::{GOOGLE_TRANSLATION_MAX_CHARACTERS, TranslationLanguage};

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
    source_characters: usize,
    source_revision: u64,
    model_output: TranslationOutput,
    google_output: TranslationOutput,
    source_language: Option<TranslationLanguage>,
    target_language: TranslationLanguage,
    pub(super) model: Option<ModelSelection>,
}

#[derive(Clone, Copy)]
enum TranslationEngine {
    Model,
    Google,
}

struct TranslationOutput {
    input: Entity<TextInput>,
    busy: bool,
    error: Option<String>,
}

impl TranslationOutput {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            input: cx.new(|cx| {
                TextInput::new(window, cx)
                    .multi_line()
                    .tab_stop()
                    .read_only(true)
            }),
            busy: false,
            error: None,
        }
    }

    fn clear(&mut self, cx: &mut App) {
        self.input.update(cx, |input, cx| input.clear(cx));
        self.error = None;
    }
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
                    .submit_on_enter()
                    .tab_stop()
                    .placeholder(tr!("translation.input"))
            }),
            source_characters: 0,
            source_revision: 0,
            model_output: TranslationOutput::new(window, cx),
            google_output: TranslationOutput::new(window, cx),
            source_language: None,
            target_language,
            model,
        }
    }

    pub(super) fn busy(&self) -> bool {
        self.model_output.busy || self.google_output.busy
    }

    pub(super) fn clear_model_result(&mut self, cx: &mut App) {
        self.model_output.clear(cx);
    }

    fn clear_results(&mut self, cx: &mut App) {
        self.clear_model_result(cx);
        self.google_output.clear(cx);
    }

    fn output(&self, engine: TranslationEngine) -> &TranslationOutput {
        match engine {
            TranslationEngine::Model => &self.model_output,
            TranslationEngine::Google => &self.google_output,
        }
    }

    fn output_mut(&mut self, engine: TranslationEngine) -> &mut TranslationOutput {
        match engine {
            TranslationEngine::Model => &mut self.model_output,
            TranslationEngine::Google => &mut self.google_output,
        }
    }

    fn google_available(&self) -> bool {
        self.source_characters <= GOOGLE_TRANSLATION_MAX_CHARACTERS
    }

    /// Check the selected model against the current service settings
    fn dialogue_model(&self, services: &[ModelService]) -> Option<&ModelSelection> {
        let selection = self.model.as_ref()?;
        services
            .iter()
            .find(|service| service.id == selection.service_id)
            .filter(|service| service.enabled)?
            .model(&selection.model)
            .filter(|model| model.options.dialogue())?;
        Some(selection)
    }
}

impl Waku {
    /// Share the active page between rendering and focus handling
    pub(super) fn open_translation_page(&self) -> Option<&TranslationPage> {
        self.translation.as_ref().filter(|page| page.open)
    }

    pub(super) fn translation_is_open(&self) -> bool {
        self.open_translation_page().is_some()
    }

    pub(super) fn translation_source_focus(&self, cx: &App) -> Option<FocusHandle> {
        self.open_translation_page()
            .map(|page| page.source.read(cx).focus_handle(cx))
    }

    pub(super) fn open_translation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.translation.get_or_insert_with(|| {
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
            cx.subscribe(
                &page.source,
                |this, input, event: &InputEvent, cx| match event {
                    InputEvent::Submit(_) => this.translate(cx),
                    InputEvent::Edited => {
                        if let Some(page) = this.translation.as_mut() {
                            page.source_characters = input.read(cx).content().chars().count();
                            page.source_revision += 1;
                            page.clear_results(cx);
                            cx.notify();
                        }
                    }
                    _ => {}
                },
            )
            .detach();
            page
        });
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

    fn translate(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self
            .translation
            .as_mut()
            .filter(|page| page.open && !page.busy())
        else {
            return;
        };
        let text = page.source.read(cx).content();
        if text.trim().is_empty() || page.source_language == Some(page.target_language) {
            return;
        }
        let model = page
            .dialogue_model(&self.state.model_services)
            .map(|model| Command::TranslateText {
                service_id: model.service_id,
                model: model.model.clone(),
                text: text.to_owned(),
                source_language: page.source_language,
                target_language: page.target_language,
            });
        let google = page
            .google_available()
            .then(|| Command::TranslateWithGoogle {
                text: text.to_owned(),
                source_language: page.source_language,
                target_language: page.target_language,
            });
        if model.is_none() && google.is_none() {
            return;
        }
        page.clear_results(cx);
        page.model_output.busy = model.is_some();
        page.google_output.busy = google.is_some();
        let source_revision = page.source_revision;
        for (engine, command) in [
            (TranslationEngine::Model, model),
            (TranslationEngine::Google, google),
        ] {
            if let Some(command) = command {
                self.request_translation(engine, command, source_revision, cx);
            }
        }
        cx.notify();
    }

    fn request_translation(
        &self,
        engine: TranslationEngine,
        command: Command,
        source_revision: u64,
        cx: &mut Context<Self>,
    ) {
        let client = self.daemon.client();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // Each engine has its own mailbox and can finish independently
                    client.request(Uuid::new_v4(), Uuid::nil(), command)
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                if let Some(page) = this.translation.as_mut() {
                    let source_matches = page.source_revision == source_revision;
                    let output = page.output_mut(engine);
                    output.busy = false;
                    // An edit while either engine is working invalidates its previous result
                    if source_matches {
                        match result {
                            Ok(ResponsePayload::Translation { text }) => output
                                .input
                                .update(cx, |input, cx| input.set_content(text, cx)),
                            Ok(_) => output.error = Some(tr!("assistant.invalid_response")),
                            Err(error) => {
                                output.error = Some(tr!("translation.failed", error = error))
                            }
                        }
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn swap_translation_languages(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.translation.as_mut().filter(|page| !page.busy()) else {
            return;
        };
        let Some(source) = page.source_language else {
            return;
        };
        page.source_language = Some(page.target_language);
        page.target_language = source;
        let result = [&page.model_output, &page.google_output]
            .into_iter()
            .map(|output| output.input.read(cx).content())
            .find(|text| !text.is_empty());
        if let Some(result) = result.map(str::to_owned) {
            page.source
                .update(cx, |input, cx| input.set_content(result, cx));
        }
        page.clear_results(cx);
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
            .outlined()
            .flex_1()
            .min_w_0()
            .justify_between()
            .selected(handle.is_open())
            .disabled(page.busy());
        if page.busy() {
            return trigger.into_any_element();
        }
        let weak = cx.weak_entity();
        dropdown_menu(trigger, id, &handle, MenuAlign::BelowLeft, move |_| {
            let choose = |language: Option<TranslationLanguage>, label: String| {
                let weak = weak.clone();
                MenuItem::new(label, move |_, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        let Some(page) = this.translation.as_mut().filter(|page| !page.busy())
                        else {
                            return;
                        };
                        if source {
                            page.source_language = language;
                        } else if let Some(language) = language {
                            page.target_language = language;
                        }
                        page.clear_results(cx);
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
        page: &TranslationPage,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let busy = page.busy();
        let same_language = page.source_language == Some(page.target_language);
        let can_translate = !busy
            && !same_language
            && !page.source.read(cx).content().trim().is_empty()
            && (page.google_available()
                || page.dialogue_model(&self.state.model_services).is_some());
        let stacked = window.viewport_size().width - px(self.sidebar_rendered_width) < px(650.0);
        let source_height =
            px((f32::from(window.viewport_size().height) * 0.31).clamp(154.0, 304.0));

        let source = self.translation_language_menu(page, true, cx);
        let target = self.translation_language_menu(page, false, cx);
        let swap = self
            .translation_button(
                "translation-swap",
                "⇄".into(),
                !busy && !same_language && page.source_language.is_some(),
                cx,
                |this, _, cx| this.swap_translation_languages(cx),
            )
            .tooltip(Tooltip::text(tr!("translation.swap")));
        let translate = self
            .translation_button(
                "translation-submit",
                if busy {
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

        let original = translation_card(theme)
            .flex_none()
            .h(source_height)
            .child(translation_heading(tr!("translation.source")))
            .child(translation_text_field(
                "translation-source-pane",
                page.source.clone(),
                theme,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(sp(12.0))
                            .text_color(if same_language {
                                theme.warning
                            } else {
                                theme.text_secondary
                            })
                            .child(if same_language {
                                tr!("translation.same_language")
                            } else {
                                format!(
                                    "{} / {GOOGLE_TRANSLATION_MAX_CHARACTERS}",
                                    page.source_characters
                                )
                            }),
                    )
                    .child(translate),
            );

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
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(20.0))
            .when(!self.sidebar_visible, |page| page.pt(px(38.0)))
            .child(
                div()
                    .flex_none()
                    .text_size(sp(18.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr!("translation.title")),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(source)
                    .child(swap)
                    .child(target),
            )
            .child(original)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(if stacked { 340.0 } else { 190.0 }))
                    .gap(px(12.0))
                    .when(stacked, |panes| panes.flex_col())
                    .child(self.render_translation_result(page, TranslationEngine::Model, cx))
                    .child(self.render_translation_result(page, TranslationEngine::Google, cx)),
            )
    }

    fn render_translation_result(
        &self,
        page: &TranslationPage,
        engine: TranslationEngine,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let output = page.output(engine);
        let (title, pane_id, copy_id, control) = match engine {
            TranslationEngine::Model => (
                tr!("translation.model_output"),
                "translation-model-result",
                "translation-copy-model",
                Some(self.render_translation_model_picker(page.model.as_ref(), page.busy(), cx)),
            ),
            TranslationEngine::Google => (
                tr!("translation.google_output"),
                "translation-google-result",
                "translation-copy-google",
                None,
            ),
        };
        let copy = self.translation_button(
            copy_id,
            tr!("common.copy"),
            !output.input.read(cx).content().is_empty(),
            cx,
            move |this, _, cx| {
                if let Some(page) = this.translation.as_ref() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        page.output(engine).input.read(cx).content().to_owned(),
                    ));
                    this.show_success_toast(tr!("translation.copied"));
                    cx.notify();
                }
            },
        );

        let is_google = matches!(engine, TranslationEngine::Google);
        let error = output.error.clone().or_else(|| {
            (is_google && !page.google_available()).then(|| {
                tr!(
                    "translation.google_limit",
                    limit = GOOGLE_TRANSLATION_MAX_CHARACTERS
                )
            })
        });

        translation_card(theme)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(6.0))
                    .child(translation_heading(title))
                    .children(control),
            )
            .when(output.busy, |card| {
                card.child(
                    div()
                        .flex_none()
                        .text_size(sp(12.0))
                        .text_color(theme.text_secondary)
                        .child(tr!("translation.translating")),
                )
            })
            .children(error.map(|error| {
                div()
                    .flex_none()
                    .text_size(sp(12.0))
                    .text_color(theme.warning)
                    .child(error)
            }))
            .child(translation_text_field(
                pane_id,
                output.input.clone(),
                theme,
                cx,
            ))
            .child(div().flex_none().flex().justify_end().child(copy))
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

fn translation_card(theme: Theme) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(12.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.border_strong)
        .bg(theme.composer)
}

fn translation_heading(label: String) -> Div {
    div()
        .flex_none()
        .text_size(sp(14.0))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label)
}

fn translation_text_field(
    id: &'static str,
    input: Entity<TextInput>,
    theme: Theme,
    cx: &App,
) -> Stateful<Div> {
    let focus = input.read(cx).focus_handle(cx);
    div()
        .id(id)
        .tab_index(0)
        .tab_stop(false)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .rounded(px(5.0))
        .border_1()
        .border_color(gpui::transparent_black())
        .in_focus(|style| style.border_color(theme.accent))
        .p(px(5.0))
        .text_size(sp(14.0))
        .line_height(sp(22.0))
        .child(input)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&focus, cx);
            // Clicking the blank part of this scroll pane must focus the input, not the pane
            window.prevent_default();
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EntityInputHandler, Modifiers, TestAppContext, point};

    struct SourcePane(Entity<TextInput>);

    impl Render for SourcePane {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(200.0))
                .h(px(120.0))
                .flex()
                .flex_col()
                .child(translation_text_field(
                    "test-translation-source",
                    self.0.clone(),
                    Theme::dark(),
                    cx,
                ))
        }
    }

    #[gpui::test]
    fn translation_accepts_input_after_clicking_below_the_text(cx: &mut TestAppContext) {
        let (pane, cx) = cx.add_window_view(|window, cx| {
            SourcePane(TranslationPage::new(window, "zh-CN", None, cx).source)
        });
        let source = cx.read_entity(&pane, |pane, _| pane.0.clone());
        // The empty input draws one line at the top; hit the surrounding blank pane
        cx.simulate_mouse_down(
            point(px(80.0), px(95.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.update(|window, cx| assert!(source.read(cx).focus_handle(cx).is_focused(window)));
        cx.simulate_input("Hello");
        cx.read_entity(&source, |input, _| assert_eq!(input.content(), "Hello"));
    }

    #[gpui::test]
    fn translation_enter_submits_and_shift_enter_inserts_a_line(cx: &mut TestAppContext) {
        cx.update(crate::input::init);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            SourcePane(TranslationPage::new(window, "zh-CN", None, cx).source)
        });
        let source = cx.read_entity(&pane, |pane, _| pane.0.clone());
        let submitted = Rc::new(RefCell::new(Vec::new()));
        cx.update(|window, cx| {
            let submitted = submitted.clone();
            cx.subscribe(&source, move |_, event: &InputEvent, _| {
                if let InputEvent::Submit(text) = event {
                    submitted.borrow_mut().push(text.clone());
                }
            })
            .detach();
            window.focus(&source.read(cx).focus_handle(cx), cx);
        });
        cx.simulate_input("Hello");
        cx.simulate_keystrokes("shift-enter");
        cx.read_entity(&source, |input, _| assert_eq!(input.content(), "Hello\n"));
        assert!(submitted.borrow().is_empty());

        cx.update(|window, cx| {
            source.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "ni", Some(2..2), window, cx);
            });
        });
        cx.simulate_keystrokes("enter");
        assert!(submitted.borrow().is_empty());
        cx.update(|window, cx| {
            source.update(cx, |input, cx| {
                input.replace_text_in_range(None, "你", window, cx)
            });
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(submitted.borrow().as_slice(), ["Hello\n你"]);
        cx.read_entity(&source, |input, _| assert_eq!(input.content(), "Hello\n你"));
    }
}
