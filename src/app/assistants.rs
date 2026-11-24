//! Desktop profile editor and entry point for native model sessions

use super::composer::next_picker_highlight;
use super::settings::SETTINGS_SPLIT_LIST_WIDTH;
use super::*;
use crate::ui::{ActivationExt as _, text_field::TextField};
use gpui::ElementId;
use waku_protocol::settings::{AssistantProfile, ServiceModel};

pub(super) const ASSISTANT_MODEL_MENU_ID: &str = "assistant-model-picker";
const ASSISTANT_DEFAULT_MODEL_MENU_ID: &str = "assistant-default-model-picker";
const ASSISTANTS_PANE_CONTEXT: &str = "AssistantsPane";
const ASSISTANTS_SEARCH_CONTEXT: &str = "AssistantsPane > TextInput";

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("down", SelectNextEntry, Some(ASSISTANTS_SEARCH_CONTEXT)),
        gpui::KeyBinding::new("up", SelectPreviousEntry, Some(ASSISTANTS_SEARCH_CONTEXT)),
    ]);
}
pub(super) const TRANSLATION_MODEL_MENU_ID: &str = "translation-model-picker";

/// A default belongs to a profile and an override belongs to one conversation
#[derive(Clone, Copy)]
enum ModelPickerTarget {
    Default,
    Session(Uuid),
    Translation,
}

impl ModelPickerTarget {
    fn menu_id(self) -> &'static str {
        match self {
            Self::Default => ASSISTANT_DEFAULT_MODEL_MENU_ID,
            Self::Session(_) => ASSISTANT_MODEL_MENU_ID,
            Self::Translation => TRANSLATION_MODEL_MENU_ID,
        }
    }

    fn accepts(self, model: &ServiceModel) -> bool {
        !matches!(self, Self::Translation) || model.options.dialogue()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ModelSelection {
    pub(super) service_id: Uuid,
    pub(super) model: String,
}

/// Keep the selected profile's draft and its list state while switching settings pages
pub(super) struct AssistantForm {
    editing: Option<Uuid>,
    name: Entity<TextInput>,
    default_model: Option<ModelSelection>,
    prompt: Entity<TextInput>,
    search: Entity<TextInput>,
    list_scroll: gpui::UniformListScrollHandle,
    detail_scroll: ScrollHandle,
}

impl AssistantForm {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut App,
        profile: Option<&AssistantProfile>,
    ) -> Self {
        let mut form = Self {
            editing: None,
            name: cx.new(|cx| TextInput::new(window, cx).tab_stop()),
            default_model: None,
            prompt: cx.new(|cx| {
                TextInput::new(window, cx)
                    .multi_line()
                    .auto_height()
                    .tab_stop()
            }),
            search: cx.new(|cx| {
                TextInput::new(window, cx)
                    .clear_on_escape()
                    .tab_stop()
                    .placeholder(tr!("assistant.search"))
            }),
            list_scroll: gpui::UniformListScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
        };
        form.load(profile, cx);
        form
    }

    pub(super) fn subscribe(&self, cx: &mut Context<Waku>) {
        cx.subscribe(&self.search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                this.assistant_form
                    .list_scroll
                    .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        })
        .detach();
    }

    pub(super) fn refresh_localized_text(&self, cx: &mut App) {
        self.search.update(cx, |input, cx| {
            input.set_placeholder(tr!("assistant.search"), cx)
        });
    }

    fn load(&mut self, profile: Option<&AssistantProfile>, cx: &mut App) {
        self.editing = profile.map(|profile| profile.id);
        self.default_model = profile.and_then(|profile| {
            profile.model_service_id.map(|service_id| ModelSelection {
                service_id,
                model: profile.model.clone(),
            })
        });
        for (input, value) in [
            (&self.name, profile.map_or("", |p| p.name.as_str())),
            (
                &self.prompt,
                profile.map_or("", |p| p.system_prompt.as_str()),
            ),
        ] {
            input.update(cx, |field, cx| field.set_content(value.to_owned(), cx));
        }
    }
}

impl Waku {
    pub(super) fn open_new_assistant_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_page(SettingsPage::Assistants, cx);
        self.new_assistant_draft(window, cx);
    }

    fn new_assistant_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.assistant_form.load(None, cx);
        self.assistant_form
            .detail_scroll
            .set_offset(gpui::Point::default());
        let focus = self.assistant_form.name.read(cx).focus_handle(cx);
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    fn select_assistant_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        // Clicking the selected row keeps the current draft in the editor
        if self.assistant_form.editing != Some(id)
            && let Some(profile) = self
                .state
                .assistant_profiles
                .iter()
                .find(|profile| profile.id == id)
        {
            self.assistant_form.load(Some(profile), cx);
            self.assistant_form
                .detail_scroll
                .set_offset(gpui::Point::default());
            cx.notify();
        }
    }

    fn visible_assistants(&self, cx: &App) -> Vec<usize> {
        let query = self
            .assistant_form
            .search
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        self.state
            .assistant_profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| {
                query.is_empty()
                    || profile.name.to_lowercase().contains(&query)
                    || profile.model.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn step_assistant_selection(&mut self, key: &str, cx: &mut Context<Self>) {
        let visible = self.visible_assistants(cx);
        let current = visible.iter().position(|index| {
            self.assistant_form.editing == Some(self.state.assistant_profiles[*index].id)
        });
        if let Some(next) = next_picker_highlight(current, visible.len(), key) {
            self.select_assistant_profile(self.state.assistant_profiles[visible[next]].id, cx);
            self.assistant_form
                .list_scroll
                .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        }
    }

    pub(super) fn render_assistants_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let form = &self.assistant_form;
        let visible = self.visible_assistants(cx);
        let list = if visible.is_empty() {
            div()
                .px(px(8.0))
                .pt(px(12.0))
                .text_size(sp(12.5))
                .text_color(theme.text_secondary)
                .child(if self.state.assistant_profiles.is_empty() {
                    tr!("assistant.empty")
                } else {
                    tr!("assistant.no_match")
                })
                .into_any_element()
        } else {
            gpui::uniform_list(
                "assistants-settings-list",
                visible.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    visible[range]
                        .iter()
                        .filter_map(|index| this.state.assistant_profiles.get(*index))
                        .map(|profile| this.render_assistant_profile_row(profile, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&form.list_scroll)
            .size_full()
            .into_any_element()
        };
        let add = assistant_button(
            "assistant-new",
            tr!("assistant.new"),
            cx,
            Self::new_assistant_draft,
        )
        .w_full()
        .justify_center();
        let left = div()
            .key_context(ASSISTANTS_PANE_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectNextEntry, _, cx| {
                this.step_assistant_selection("down", cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousEntry, _, cx| {
                this.step_assistant_selection("up", cx);
            }))
            .w(px(SETTINGS_SPLIT_LIST_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .pt(px(22.0))
                    .pb(px(8.0))
                    .child(
                        TextField::new("assistant-search", form.search.clone())
                            .icon("icons/search.svg", 13.0)
                            .w_full(),
                    ),
            )
            .child(div().flex_1().min_h_0().px(px(8.0)).child(list))
            .child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(add),
            );
        div()
            .size_full()
            .min_h_0()
            .flex()
            .child(left)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(self.render_assistant_detail(cx)),
            )
            .into_any_element()
    }

    fn render_assistant_profile_row(
        &self,
        profile: &AssistantProfile,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let id = profile.id;
        let selected = self.assistant_form.editing == Some(id);
        div().h(px(56.0)).py(px(2.0)).w_full().child(
            div()
                .id(SharedString::from(format!("assistant-profile-{id}")))
                .tab_index(0)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .size_full()
                .px(px(9.0))
                .rounded(px(8.0))
                .cursor_default()
                .flex()
                .items_center()
                .gap(px(9.0))
                .when(selected, |row| row.bg(theme.sidebar_item_background))
                .when(!selected, |row| row.hover(|style| style.bg(theme.overlay)))
                .child(assistant_profile_icon(theme, 26.0, 13.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(profile.name.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(sp(12.5))
                                .text_color(theme.text_tertiary)
                                .child(profile.model.clone()),
                        ),
                )
                .on_activation(cx, move |this, _, cx| this.select_assistant_profile(id, cx)),
        )
    }

    fn render_assistant_detail(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let form = &self.assistant_form;
        let profile = form.editing.and_then(|id| {
            self.state
                .assistant_profiles
                .iter()
                .find(|profile| profile.id == id)
        });
        let project = self
            .selected_project()
            .filter(|project| !project.is_projectless());
        let header = div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(assistant_profile_icon(theme, 38.0, 18.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(sp(15.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(profile.map_or_else(
                                || tr!("assistant.new"),
                                |profile| profile.name.clone(),
                            )),
                    )
                    .when_some(project, |title, project| {
                        title.child(
                            div()
                                .mt(px(3.0))
                                .truncate()
                                .text_size(sp(12.5))
                                .text_color(theme.text_tertiary)
                                .child(tr!(
                                    "assistant.current_project",
                                    project = project.name.as_str()
                                )),
                        )
                    }),
            )
            .when_some(profile.map(|profile| profile.id), |header, id| {
                header.child(assistant_button(
                    "assistant-start",
                    tr!("assistant.start"),
                    cx,
                    move |this, window, cx| {
                        if let Some(profile) = this
                            .state
                            .assistant_profiles
                            .iter()
                            .find(|item| item.id == id)
                            .cloned()
                        {
                            this.start_assistant(profile, window, cx);
                        }
                    },
                ))
            });
        div()
            .id("assistant-detail-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&form.detail_scroll)
            .px(px(24.0))
            .pt(px(18.0))
            .pb(px(20.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(header)
            .child(assistant_field(
                tr!("assistant.name"),
                TextField::new("assistant-name", form.name.clone()),
                theme,
            ))
            .child(assistant_field(
                tr!("assistant.default_model"),
                self.render_assistant_model_picker(
                    ModelPickerTarget::Default,
                    form.default_model.as_ref(),
                    self.model_selection_label(form.default_model.as_ref()),
                    cx,
                ),
                theme,
            ))
            .child(assistant_field(
                tr!("assistant.prompt"),
                div()
                    .min_h(px(70.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.inset)
                    .text_size(sp(13.0))
                    .line_height(sp(18.0))
                    .child(form.prompt.clone()),
                theme,
            ))
            .child(assistant_button(
                "assistant-save",
                tr!("assistant.save"),
                cx,
                |this, _, cx| this.save_assistant(cx),
            ))
    }

    pub(super) fn assistant_model_picker_open(&self) -> bool {
        let menus = self.menus.borrow();
        [
            ASSISTANT_MODEL_MENU_ID,
            ASSISTANT_DEFAULT_MODEL_MENU_ID,
            TRANSLATION_MODEL_MENU_ID,
        ]
        .into_iter()
        .any(|id| menus.get(id).is_some_and(ContextMenuHandle::is_open))
    }

    pub(super) fn render_session_assistant_model(
        &self,
        session: &AgentSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(profile) = session.assistant.as_ref() else {
            return div()
                .child(tr!("assistant.choose_model"))
                .into_any_element();
        };
        let selection = profile.model_service_id.map(|service_id| ModelSelection {
            service_id,
            model: profile.model.clone(),
        });
        let label = selection.as_ref().map_or_else(
            || format!("{} · {}", profile.name, profile.model),
            |selection| self.model_selection_label(Some(selection)),
        );
        if session.status.is_busy() {
            return div()
                .h(px(24.0))
                .flex()
                .items_center()
                .px(px(7.0))
                .max_w(px(210.0))
                .truncate()
                .text_color(Theme::current(cx).text_secondary)
                .child(label)
                .into_any_element();
        }
        self.render_assistant_model_picker(
            ModelPickerTarget::Session(session.id),
            selection.as_ref(),
            label,
            cx,
        )
    }

    pub(super) fn render_translation_model_picker(
        &self,
        model: Option<&ModelSelection>,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = self.model_selection_label(model);
        if busy {
            return MenuChip::new(TRANSLATION_MODEL_MENU_ID)
                .label(label)
                .disabled(true)
                .into_any_element();
        }
        self.render_assistant_model_picker(ModelPickerTarget::Translation, model, label, cx)
    }

    fn model_selection_label(&self, selection: Option<&ModelSelection>) -> String {
        let Some(selection) = selection else {
            return tr!("assistant.choose_model");
        };
        if let Some(service) = self
            .model_service(selection.service_id)
            .filter(|service| service.enabled)
            && let Some(model) = service.model(&selection.model)
        {
            format!("{} · {}", service.name, model.display_name())
        } else {
            tr!(
                "assistant.model_unavailable",
                model = selection.model.as_str()
            )
        }
    }

    /// Reuse the composer's search state, popover keys and highlighted-row navigation
    fn render_assistant_model_picker(
        &self,
        target: ModelPickerTarget,
        selection: Option<&ModelSelection>,
        label: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let search = self.model_search.clone();
        let weak = cx.weak_entity();
        let scroll = self.assistant_model_scroll.clone();
        let manage_focus = self.model_picker_empty_focus.clone();
        let handle = {
            let weak = weak.clone();
            let search = search.clone();
            let scroll = scroll.clone();
            self.menu_handle_with(target.menu_id(), cx, move |open, window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if open {
                        search.update(cx, |input, cx| input.clear(cx));
                        this.model_picker_highlight = None;
                        scroll.scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
                    } else {
                        let focus = match target {
                            ModelPickerTarget::Default => this.settings_focus.clone(),
                            ModelPickerTarget::Session(_) => this.composer_focus(cx),
                            ModelPickerTarget::Translation => this
                                .translation_source_focus(cx)
                                .unwrap_or_else(|| this.composer_focus(cx)),
                        };
                        window.focus(&focus, cx);
                    }
                    cx.notify();
                });
                if open {
                    let focus = search.read(cx).focus_handle(cx);
                    window.on_next_frame(move |window, _| {
                        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
                    });
                }
            })
        };
        let rows = Rc::new(if handle.is_open() {
            let query = search.read(cx).content().trim().to_lowercase();
            let mut rows = Vec::new();
            for service in self
                .state
                .model_services
                .iter()
                .filter(|service| service.enabled)
            {
                let provider_matches =
                    query.is_empty() || service.name.to_lowercase().contains(&query);
                for model in service.models.iter().filter(|model| target.accepts(model)) {
                    if provider_matches
                        || model.id.to_lowercase().contains(&query)
                        || model.name.to_lowercase().contains(&query)
                        || model.options.group.to_lowercase().contains(&query)
                    {
                        rows.push((
                            if model.options.group.is_empty() {
                                service.name.clone()
                            } else {
                                format!("{} · {}", service.name, model.options.group)
                            },
                            model.display_name().to_owned(),
                            ModelSelection {
                                service_id: service.id,
                                model: model.id.clone(),
                            },
                        ));
                    }
                }
            }
            rows
        } else {
            Vec::new()
        });
        let selection = selection.cloned().map(|mut selection| {
            if let Some(model) = self
                .model_service(selection.service_id)
                .and_then(|service| service.model(&selection.model))
            {
                selection.model.clone_from(&model.id);
            }
            selection
        });
        let highlight = self.model_picker_highlight;
        let has_catalog = self.state.model_services.iter().any(|service| {
            service.enabled && service.models.iter().any(|model| target.accepts(model))
        });
        let trigger = MenuChip::new(target.menu_id())
            .provider(ProviderKind::Assistant, theme.accent)
            .label(label)
            .selected(handle.is_open());
        let align = match target {
            ModelPickerTarget::Default | ModelPickerTarget::Translation => MenuAlign::BelowLeft,
            ModelPickerTarget::Session(_) => MenuAlign::AboveLeft,
        };
        popover(trigger, &handle, align, move |popover, _, _| {
            let move_highlight = {
                let count = rows.len();
                let weak = weak.clone();
                let scroll = scroll.clone();
                move |key, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        let current = this.model_picker_highlight.filter(|index| *index < count);
                        if let Some(index) =
                            super::composer::next_picker_highlight(current, count, key)
                        {
                            this.model_picker_highlight = Some(index);
                            scroll.scroll_to_item(index, gpui::ScrollStrategy::Nearest);
                            cx.notify();
                        }
                    });
                }
            };
            let choose = {
                let weak = weak.clone();
                let popover = popover.clone();
                move |selection: ModelSelection, window: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        this.select_assistant_model(target, selection, cx)
                    });
                    popover.close(window, cx);
                }
            };
            let list = {
                let rows = rows.clone();
                let count = rows.len();
                let choose = choose.clone();
                let selection = selection.clone();
                gpui::uniform_list("assistant-model-rows", rows.len(), move |range, _, _| {
                    range
                        .map(|index| {
                            let (supplier, name, model) = &rows[index];
                            let is_selected = selection.as_ref() == Some(model);
                            let model_for_click = model.clone();
                            let select = choose.clone();
                            div()
                                .id(index)
                                .h(px(46.0))
                                .px(px(10.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(if highlight == Some(index) {
                                    theme.accent
                                } else {
                                    gpui::transparent_black()
                                })
                                .when(is_selected, |row| row.bg(theme.overlay_strong))
                                .hover(|row| row.bg(theme.overlay))
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .child(div().truncate().child(name.clone()))
                                        .child(
                                            div()
                                                .truncate()
                                                .text_size(sp(11.0))
                                                .text_color(theme.text_secondary)
                                                .child(supplier.clone()),
                                        ),
                                )
                                .when(is_selected, |row| {
                                    row.child(icon("icons/check.svg", 14.0, theme.accent))
                                })
                                .on_click(move |_, window, cx| {
                                    select(model_for_click.clone(), window, cx)
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .track_scroll(&scroll)
                .w_full()
                .h(px((count.clamp(1, 6) * 46) as f32))
            };
            let next = move_highlight.clone();
            let confirmed = rows
                .get(highlight.unwrap_or(0))
                .map(|(_, _, model)| model.clone());
            let configure = {
                let weak = weak.clone();
                let popover = popover.clone();
                move |window: &mut Window, cx: &mut App| {
                    popover.close(window, cx);
                    let _ = weak.update(cx, |this, cx| match target {
                        ModelPickerTarget::Session(id) => {
                            this.open_model_service_editor(None, Some(id), window, cx)
                        }
                        ModelPickerTarget::Default | ModelPickerTarget::Translation => {
                            this.open_settings_page(SettingsPage::ModelServices, cx)
                        }
                    });
                }
            };
            let keyboard_configure = configure.clone();
            let settings_focus = manage_focus.clone();
            div()
                .w(px(330.0))
                .p(px(8.0))
                .gap(px(6.0))
                .flex()
                .flex_col()
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.raised)
                .shadow_lg()
                .on_action(move |_: &SelectNextEntry, _, cx| next("down", cx))
                .on_action(move |_: &SelectPreviousEntry, _, cx| move_highlight("up", cx))
                .on_action(move |_: &SelectNextTab, window, cx| window.focus(&settings_focus, cx))
                .on_action(move |_: &ConfirmEntry, window, cx| {
                    if let Some(selection) = confirmed.clone() {
                        choose(selection, window, cx);
                    }
                })
                .child(
                    TextField::new("assistant-model-search", search.clone())
                        .icon("icons/search.svg", 13.0),
                )
                .when(matches!(target, ModelPickerTarget::Session(_)), |panel| {
                    panel.child(
                        div()
                            .px(px(6.0))
                            .text_size(sp(11.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("assistant.switch_history")),
                    )
                })
                .when(!rows.is_empty(), |panel| panel.child(list))
                .when(rows.is_empty(), |panel| {
                    panel.child(div().px(px(6.0)).text_color(theme.text_secondary).child(
                        if has_catalog {
                            tr!("models.none_found")
                        } else {
                            tr!("model_services.no_models")
                        },
                    ))
                })
                .child(
                    div()
                        .id("open-model-services")
                        .track_focus(&manage_focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .h(px(28.0))
                        .px(px(6.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .text_color(theme.accent)
                        .focus_visible(|row| row.bg(theme.overlay))
                        .hover(|row| row.bg(theme.overlay))
                        .child(match target {
                            ModelPickerTarget::Session(_) => tr!("model_services.add"),
                            ModelPickerTarget::Default | ModelPickerTarget::Translation => {
                                tr!("model_services.manage")
                            }
                        })
                        .on_key_down(move |event, window, cx| {
                            if !event.keystroke.modifiers.modified()
                                && matches!(event.keystroke.key.as_str(), "enter" | "space")
                            {
                                keyboard_configure(window, cx);
                                cx.stop_propagation();
                            }
                        })
                        .on_click(move |_, window, cx| configure(window, cx)),
                )
                .into_any_element()
        })
    }

    pub(super) fn use_assistant_service_model(
        &mut self,
        session: Uuid,
        service_id: Uuid,
        model: String,
        cx: &mut Context<Self>,
    ) -> bool {
        self.select_assistant_model(
            ModelPickerTarget::Session(session),
            ModelSelection { service_id, model },
            cx,
        )
    }

    fn select_assistant_model(
        &mut self,
        target: ModelPickerTarget,
        selection: ModelSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .model_service(selection.service_id)
            .is_none_or(|service| {
                !service.enabled
                    || !service
                        .model(&selection.model)
                        .is_some_and(|model| target.accepts(model))
            })
        {
            return false;
        }
        match target {
            ModelPickerTarget::Default => self.assistant_form.default_model = Some(selection),
            ModelPickerTarget::Translation => {
                let Some(page) = self.translation.as_mut().filter(|page| !page.busy()) else {
                    return false;
                };
                page.model = Some(selection);
                page.clear_model_result(cx);
            }
            ModelPickerTarget::Session(id) => {
                if self.state.selected_session != Some(id) {
                    return false;
                }
                let Some(session) = self.state.session_mut(id) else {
                    return false;
                };
                if session.status.is_busy() {
                    return false;
                }
                let Some(profile) = session.assistant.as_mut() else {
                    return false;
                };
                if profile.model_service_id == Some(selection.service_id)
                    && profile.model == selection.model
                {
                    return true;
                }
                profile.model_service_id = Some(selection.service_id);
                profile.model = selection.model.clone();
                profile.base_url.clear();
                session.model = Some(selection.model);
                session.context_usage = None;
                // A new runtime replays the saved dialogue at the new supplier
                self.runtime_attach_pending.remove(&id);
                self.reset_session_runtime(id);
                self.save();
            }
        }
        cx.notify();
        true
    }

    fn save_assistant(&mut self, cx: &mut Context<Self>) {
        let form = &self.assistant_form;
        let Some(selection) = form.default_model.clone() else {
            self.show_toast(tr!("assistant.choose_model"));
            cx.notify();
            return;
        };
        let profile = AssistantProfile {
            id: form.editing.unwrap_or_else(Uuid::new_v4),
            name: form.name.read(cx).content().trim().to_owned(),
            model: selection.model,
            web_search: self
                .state
                .assistant_profiles
                .iter()
                .any(|profile| Some(profile.id) == form.editing && profile.web_search),
            model_service_id: Some(selection.service_id),
            base_url: String::new(),
            system_prompt: form.prompt.read(cx).content().to_owned(),
        };
        if profile.name.is_empty() {
            self.show_toast(tr!("assistant.fields_required"));
            cx.notify();
            return;
        }
        let creating = form.editing.is_none();
        self.assistant_form.load(Some(&profile), cx);
        if creating {
            self.assistant_form
                .search
                .update(cx, |input, cx| input.clear(cx));
        }
        if let Some(saved) = self
            .state
            .assistant_profiles
            .iter_mut()
            .find(|item| item.id == profile.id)
        {
            *saved = profile;
        } else {
            self.state.assistant_profiles.push(profile);
        }
        self.save();
        cx.notify();
    }

    pub(super) fn start_assistant(
        &mut self,
        profile: AssistantProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project_id = self
            .selected_project()
            .filter(|project| !project.is_projectless())
            .map(|p| p.id);
        self.settings_page = None;
        if let Some(project_id) = project_id {
            self.create_assistant_task(project_id, profile, cx);
        } else {
            self.create_projectless_task(Some(profile), cx);
        }
        window.focus(&self.composer_focus(cx), cx);
    }

    /// Start a task with its own profile snapshot so subsequent edits affect new tasks
    pub(super) fn create_assistant_task(
        &mut self,
        project_id: Uuid,
        profile: AssistantProfile,
        cx: &mut Context<Self>,
    ) {
        let mut session = AgentSession::new(project_id, ProviderKind::Assistant);
        session.auto_title = Some(profile.name.clone());
        session.assistant_id = Some(profile.id);
        self.set_sidebar_group_collapsed(SidebarGroup::Assistant(profile.id), false, cx);
        session.model = Some(profile.model.clone());
        session.runtime_mode = RuntimeMode::Ask;
        session.assistant = Some(profile);
        let id = session.id;
        self.state.push_session(session);
        self.select_session(id, cx);
    }
}

fn assistant_profile_icon(theme: Theme, size: f32, icon_size: f32) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(size * 0.24))
        .bg(theme.overlay)
        .flex()
        .items_center()
        .justify_center()
        .child(icon("icons/bot.svg", icon_size, theme.text_secondary))
}

pub(super) fn assistant_field(label: String, input: impl IntoElement, theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .child(
            div()
                .text_size(sp(12.0))
                .text_color(theme.text_secondary)
                .child(label),
        )
        .child(input)
}

pub(super) fn assistant_button(
    id: impl Into<ElementId>,
    label: String,
    cx: &mut Context<Waku>,
    action: impl Fn(&mut Waku, &mut Window, &mut Context<Waku>) + 'static,
) -> Stateful<Div> {
    let theme = Theme::current(cx);
    assistant_button_base(id, label, theme)
        .hover(|style| style.bg(theme.overlay))
        .on_activation(cx, action)
}

/// Share sizing and focus styling while each button sets hover once
pub(super) fn assistant_button_base(
    id: impl Into<ElementId>,
    label: String,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .tab_index(0)
        .focus_visible(|style| style.border_color(theme.accent))
        .h(px(28.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .self_start()
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border_strong)
        .text_size(sp(12.0))
        .child(label)
}
