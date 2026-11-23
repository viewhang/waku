//! Desktop profile editor and entry point for native model sessions

use super::*;
use crate::ui::{ActivationExt as _, text_field::TextField};
use gpui::ElementId;
use waku_protocol::settings::AssistantProfile;

pub(super) const ASSISTANT_MODEL_MENU_ID: &str = "assistant-model-picker";
const ASSISTANT_DEFAULT_MODEL_MENU_ID: &str = "assistant-default-model-picker";

/// A default belongs to a profile and an override belongs to one conversation
#[derive(Clone, Copy)]
enum ModelPickerTarget {
    Default,
    Session(Uuid),
}

impl ModelPickerTarget {
    fn menu_id(self) -> &'static str {
        match self {
            Self::Default => ASSISTANT_DEFAULT_MODEL_MENU_ID,
            Self::Session(_) => ASSISTANT_MODEL_MENU_ID,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ModelSelection {
    service_id: Uuid,
    model: String,
}

/// Keep editable text in input entities and keep credentials outside the form
pub(super) struct AssistantForm {
    editing: Option<Uuid>,
    name: Entity<TextInput>,
    default_model: Option<ModelSelection>,
    prompt: Entity<TextInput>,
}

impl AssistantForm {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            editing: None,
            name: cx.new(|cx| TextInput::new(window, cx)),
            default_model: None,
            prompt: cx.new(|cx| TextInput::new(window, cx).multi_line().auto_height()),
        }
    }

    fn load(&mut self, profile: Option<&AssistantProfile>, cx: &mut Context<Waku>) {
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
        cx.notify();
    }
}

impl Waku {
    pub(super) fn open_new_assistant_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_page(SettingsPage::Assistants, cx);
        self.assistant_form.load(None, cx);
        window.focus(&self.assistant_form.name.read(cx).focus_handle(cx), cx);
    }

    pub(super) fn render_assistants_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let form = &self.assistant_form;
        let project = self
            .selected_project()
            .filter(|project| !project.is_projectless());
        let mut list = div().flex().flex_col().gap(px(8.0));
        for profile in &self.state.assistant_profiles {
            let edit_profile = profile.clone();
            let start_profile = profile.clone();
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(profile.name.clone())
                            .child(
                                div()
                                    .text_size(sp(12.0))
                                    .text_color(theme.text_secondary)
                                    .child(profile.model.clone()),
                            ),
                    )
                    .child(assistant_button(
                        SharedString::from(format!("assistant-edit-{}", profile.id)),
                        tr!("assistant.edit"),
                        cx,
                        move |this, _, cx| this.assistant_form.load(Some(&edit_profile), cx),
                    ))
                    .child(assistant_button(
                        SharedString::from(format!("assistant-start-{}", profile.id)),
                        tr!("assistant.start"),
                        cx,
                        move |this, window, cx| {
                            this.start_assistant(start_profile.clone(), window, cx)
                        },
                    )),
            );
        }
        div()
            .mt(px(16.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .when_some(project, |body, project| {
                body.child(
                    div()
                        .text_size(sp(12.0))
                        .text_color(theme.text_secondary)
                        .child(tr!(
                            "assistant.current_project",
                            project = project.name.as_str()
                        )),
                )
            })
            .child(list)
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
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(assistant_button(
                        "assistant-save",
                        tr!("assistant.save"),
                        cx,
                        |this, _, cx| {
                            this.save_assistant(cx);
                        },
                    ))
                    .when(form.editing.is_some(), |element| {
                        element.child(assistant_button(
                            "assistant-new",
                            tr!("assistant.new"),
                            cx,
                            |this, _, cx| this.assistant_form.load(None, cx),
                        ))
                    }),
            )
            .into_any_element()
    }

    pub(super) fn assistant_model_picker_open(&self) -> bool {
        let menus = self.menus.borrow();
        [ASSISTANT_MODEL_MENU_ID, ASSISTANT_DEFAULT_MODEL_MENU_ID]
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
                for model in &service.models {
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
        let has_catalog = self
            .state
            .model_services
            .iter()
            .any(|service| service.enabled && !service.models.is_empty());
        let trigger = MenuChip::new(target.menu_id())
            .provider(ProviderKind::Assistant, theme.accent)
            .label(label)
            .selected(handle.is_open());
        let align = match target {
            ModelPickerTarget::Default => MenuAlign::BelowLeft,
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
                        ModelPickerTarget::Default => {
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
                            ModelPickerTarget::Default => tr!("model_services.manage"),
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
            .is_none_or(|service| !service.enabled || service.model(&selection.model).is_none())
        {
            return false;
        }
        match target {
            ModelPickerTarget::Default => self.assistant_form.default_model = Some(selection),
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
            model_service_id: Some(selection.service_id),
            base_url: String::new(),
            system_prompt: form.prompt.read(cx).content().to_owned(),
        };
        if profile.name.is_empty() {
            self.show_toast(tr!("assistant.fields_required"));
            cx.notify();
            return;
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
        self.assistant_form.load(None, cx);
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
