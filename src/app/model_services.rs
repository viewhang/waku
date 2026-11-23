//! Shared editor for model service settings and adding a service from a conversation

use super::assistants::{assistant_button, assistant_button_base, assistant_field};
use super::*;
use crate::ui::ActivationExt as _;
use anyhow::Context as _;
use waku_client::{Command, ResponsePayload};
use waku_protocol::settings::{
    ModelKind, ModelModality, ModelProtocol, ModelPurpose, ModelService, ServiceModel,
    ServiceModelOptions,
};

const PRESETS: [(&str, &str); 6] = [
    ("OpenAI", "https://api.openai.com/v1"),
    ("OpenRouter", "https://openrouter.ai/api/v1"),
    ("Ollama", "http://localhost:11434/v1"),
    ("LM Studio", "http://localhost:1234/v1"),
    ("Anthropic", "https://api.anthropic.com/v1"),
    ("Gemini", "https://generativelanguage.googleapis.com/v1beta"),
];
const DIALOG: &str = "ModelServiceEditor";

gpui::actions!(model_service_editor, [CloseModelServiceEditor]);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new(
        "escape",
        CloseModelServiceEditor,
        Some(DIALOG),
    )]);
}

#[derive(Clone)]
enum ModelEdit {
    Add,
    Replace(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    LoadKey,
    Check,
    Sync,
    Save,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NoticeAt {
    Address,
    Key,
    Catalog,
}

/// Keep connection checks and model edits on the draft until the user confirms
pub(super) struct ModelServiceForm {
    generation: Uuid,
    session: Option<Uuid>,
    original: Option<(Uuid, String)>,
    name: Entity<TextInput>,
    url: Entity<TextInput>,
    key: Entity<TextInput>,
    original_key: Option<SharedString>,
    enabled: bool,
    models: Rc<Vec<ServiceModel>>,
    selected: Option<String>,
    model_edit: Option<ModelEdit>,
    model_id: Entity<TextInput>,
    model_name: Entity<TextInput>,
    model_group: Entity<TextInput>,
    model_options: ServiceModelOptions,
    model_context: Entity<TextInput>,
    model_input_tokens: Entity<TextInput>,
    model_output_tokens: Entity<TextInput>,
    model_search: Entity<TextInput>,
    work: Option<Work>,
    status: Option<(NoticeAt, bool, String)>,
    scroll: gpui::UniformListScrollHandle,
}

impl Waku {
    pub(super) fn render_model_services_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let mut services = div().mt(px(16.0)).flex().flex_col().gap(px(10.0));
        for service in &self.state.model_services {
            let id = service.id;
            services = services.child(
                div()
                    .px(px(12.0))
                    .py(px(9.0))
                    .rounded(px(8.0))
                    .bg(theme.inset)
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .child(div().truncate().child(service.name.clone()))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(sp(12.0))
                                    .text_color(theme.text_secondary)
                                    .child(service.base_url.clone()),
                            ),
                    )
                    .child(assistant_button(
                        SharedString::from(format!("edit-service-{id}")),
                        tr!("assistant.edit"),
                        cx,
                        move |this, window, cx| {
                            this.open_model_service_editor(Some(id), None, window, cx)
                        },
                    )),
            );
        }
        services
            .child(assistant_button(
                "add-model-service",
                tr!("model_services.add"),
                cx,
                |this, window, cx| this.open_model_service_editor(None, None, window, cx),
            ))
            .into_any_element()
    }

    pub(super) fn model_service(&self, id: Uuid) -> Option<&ModelService> {
        self.state
            .model_services
            .iter()
            .find(|service| service.id == id)
    }

    pub(super) fn open_model_service_editor(
        &mut self,
        service_id: Option<Uuid>,
        session: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = service_id.and_then(|id| self.model_service(id)).cloned();
        let generation = Uuid::new_v4();
        let name = cx.new(|cx| {
            let mut input = TextInput::new(window, cx);
            input.set_content(
                saved.as_ref().map_or("", |service| service.name.as_str()),
                cx,
            );
            input
        });
        let focus = name.read(cx).focus_handle(cx);
        let url = cx.new(|cx| {
            let mut input = TextInput::new(window, cx).placeholder(PRESETS[0].1);
            input.set_content(
                saved
                    .as_ref()
                    .map_or("", |service| service.base_url.as_str()),
                cx,
            );
            input
        });
        let load_key = cfg!(target_os = "macos") && !self.daemon.is_remote() && saved.is_some();
        let model_search = cx
            .new(|cx| TextInput::new(window, cx).placeholder(tr!("model_services.search_models")));
        cx.subscribe(
            &model_search,
            |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Edited) {
                    if let Some(form) = &this.model_service_form {
                        form.scroll
                            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
                    }
                    cx.notify();
                }
            },
        )
        .detach();
        self.model_service_form = Some(ModelServiceForm {
            generation,
            session,
            original: saved
                .as_ref()
                .map(|service| (service.id, service.base_url.clone())),
            name,
            url,
            key: cx.new(|cx| TextInput::new(window, cx).password()),
            original_key: None,
            enabled: saved.as_ref().is_none_or(|service| service.enabled),
            models: Rc::new(
                saved
                    .as_ref()
                    .map(|service| service.models.clone())
                    .unwrap_or_default(),
            ),
            selected: None,
            model_edit: None,
            model_id: cx.new(|cx| TextInput::new(window, cx)),
            model_name: cx.new(|cx| TextInput::new(window, cx)),
            model_group: cx.new(|cx| TextInput::new(window, cx)),
            model_options: ServiceModelOptions::default(),
            model_context: cx.new(|cx| TextInput::new(window, cx).placeholder("128000")),
            model_input_tokens: cx.new(|cx| TextInput::new(window, cx).placeholder("128000")),
            model_output_tokens: cx.new(|cx| TextInput::new(window, cx).placeholder("65536")),
            model_search,
            work: load_key.then_some(Work::LoadKey),
            status: None,
            scroll: gpui::UniformListScrollHandle::new(),
        });
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
        cx.notify();

        if let Some(service) = saved.filter(|_| load_key) {
            cx.spawn(async move |view, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { read_local_key(&service) })
                    .await;
                let _ = view.update(cx, |this, cx| {
                    let Some(form) = this
                        .model_service_form
                        .as_mut()
                        .filter(|form| form.generation == generation)
                    else {
                        return;
                    };
                    form.work = None;
                    match result {
                        Ok(Some(key)) => {
                            let key = SharedString::from(key);
                            form.original_key = Some(key.clone());
                            form.key.update(cx, |input, cx| input.set_content(key, cx));
                        }
                        Ok(None) => {}
                        Err(_) => {
                            form.status =
                                Some((NoticeAt::Key, true, tr!("model_services.key_read_failed")))
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    pub(super) fn render_model_service_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let form = self.model_service_form.as_ref()?;
        if let Some(edit) = &form.model_edit {
            return Some(self.render_service_model_editor(form, edit, cx));
        }
        let theme = Theme::current(cx);
        let presets = PRESETS.into_iter().fold(
            div().flex().flex_wrap().gap(px(6.0)),
            |row, (name, endpoint)| {
                row.child(assistant_button(
                    name,
                    name.to_owned(),
                    cx,
                    move |this, _, cx| {
                        if let Some(form) = this.model_service_form.as_mut() {
                            form.name
                                .update(cx, |input, cx| input.set_content(name, cx));
                            form.url
                                .update(cx, |input, cx| input.set_content(endpoint, cx));
                            form.status = None;
                            cx.notify();
                        }
                    },
                ))
            },
        );
        let key_revealed = form.key.read(cx).password_revealed();
        let key_label = if key_revealed {
            tr!("model_services.hide_key")
        } else {
            tr!("model_services.show_key")
        };
        let key_field = form.key.clone();
        let can_request = form.work.is_none();
        let can_save = can_request && (form.session.is_none() || form.selected.is_some());
        let api_key_label = if form
            .url
            .read(cx)
            .content()
            .trim_start()
            .get(..7)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
        {
            tr!("model_services.api_key_optional")
        } else {
            tr!("model_services.api_key")
        };
        let mut key_row = div().flex().items_center().gap(px(6.0));
        if form.work == Some(Work::LoadKey) {
            key_row = key_row.child(tr!("model_services.reading_key"));
        } else {
            let eye = div()
                .id("reveal-model-service-key")
                .tab_index(0)
                .tab_stop(true)
                .size(px(23.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .hover(|button| button.bg(theme.overlay))
                .focus_visible(|button| button.bg(theme.overlay_strong))
                .tooltip(Tooltip::text(key_label))
                .child(icon(
                    if key_revealed {
                        "icons/eye-off.svg"
                    } else {
                        "icons/eye.svg"
                    },
                    14.0,
                    theme.text_secondary,
                ))
                .on_activation(cx, {
                    let key_field = key_field.clone();
                    move |_, window, cx| {
                        key_field.update(cx, |input, cx| {
                            input.set_password_revealed(!key_revealed, cx)
                        });
                        window.focus(&key_field.read(cx).focus_handle(cx), cx);
                        cx.notify();
                    }
                });
            key_row = key_row
                .child(
                    TextField::new("model-service-key", key_field)
                        .trailing(eye)
                        .flex_1()
                        .min_w_0(),
                )
                .when(can_request, |row| {
                    row.child(assistant_button(
                        "check-model-service",
                        tr!("model_services.test"),
                        cx,
                        |this, _, cx| this.request_model_service(Work::Check, cx),
                    ))
                });
        }
        let heading = if form.original.is_some() {
            tr!("model_services.edit")
        } else {
            tr!("model_services.add")
        };
        let content = div()
            .id("model-service-editor-scroll")
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .when(form.original.is_none(), |body| body.child(presets))
            .child(assistant_field(
                tr!("assistant.name"),
                TextField::new("model-service-name", form.name.clone()),
                theme,
            ))
            .child(assistant_field(
                tr!("model_services.endpoint"),
                TextField::new("model-service-url", form.url.clone()),
                theme,
            ))
            .children(form.notice(NoticeAt::Address, theme))
            .child(assistant_field(api_key_label, key_row, theme))
            .children(form.notice(NoticeAt::Key, theme))
            .when(form.session.is_none(), |body| {
                body.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(tr!("model_services.enabled"))
                        .child(toggle_switch(
                            "model-service-enabled",
                            form.enabled,
                            !can_request,
                            theme,
                            cx,
                            |this, _, cx| {
                                if let Some(form) = this.model_service_form.as_mut() {
                                    form.enabled = !form.enabled;
                                    cx.notify();
                                }
                            },
                        )),
                )
            })
            .child(self.render_model_service_catalog(form, cx))
            .children(form.notice(NoticeAt::Catalog, theme));

        let card = div()
            .id("model-service-editor")
            .key_context(DIALOG)
            .on_action(
                cx.listener(|this, _: &CloseModelServiceEditor, window, cx| {
                    this.close_model_service_editor(window, cx)
                }),
            )
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(560.0))
            .max_h(gpui::relative(1.0))
            .flex()
            .flex_col()
            .rounded(px(14.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface)
            .text_color(theme.text)
            .shadow_xl()
            .child(
                div()
                    .px(px(16.0))
                    .py(px(12.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(sp(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(heading),
            )
            .child(content)
            .child(
                div()
                    .px(px(16.0))
                    .py(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .when(form.session.is_some(), |footer| {
                        footer.child(
                            div()
                                .text_size(sp(11.0))
                                .text_color(theme.text_secondary)
                                .child(tr!("assistant.switch_history")),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .when(form.work != Some(Work::Save), |row| {
                                row.child(assistant_button(
                                    "cancel-model-service",
                                    tr!("model_services.cancel"),
                                    cx,
                                    |this, window, cx| this.close_model_service_editor(window, cx),
                                ))
                            })
                            .child(
                                model_service_save_button(
                                    if form.session.is_some() {
                                        tr!("model_services.add_and_use")
                                    } else {
                                        tr!("model_services.save")
                                    },
                                    can_save,
                                    theme,
                                )
                                .on_activation(
                                    cx,
                                    move |this, window, cx| {
                                        if can_save {
                                            this.save_model_service(window, cx);
                                        }
                                    },
                                ),
                            )
                            .when(form.work.is_some_and(|work| work != Work::LoadKey), |row| {
                                row.child(tr!("model_services.working"))
                            }),
                    ),
            );
        Some(model_service_dialog_layer(card))
    }

    fn render_service_model_editor(
        &self,
        form: &ModelServiceForm,
        edit: &ModelEdit,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let (title, action) = match edit {
            ModelEdit::Add => (
                tr!("model_services.add_model"),
                tr!("model_services.add_model"),
            ),
            ModelEdit::Replace(_) => (
                tr!("model_services.edit_model"),
                tr!("model_services.save_model"),
            ),
        };
        let cancel = tr!("model_services.cancel");
        model_service_dialog_layer(
            div()
                .id("service-model-editor")
                .key_context(DIALOG)
                .on_action(
                    cx.listener(|this, _: &CloseModelServiceEditor, window, cx| {
                        this.close_model_service_editor(window, cx);
                    }),
                )
                .tab_group()
                .tab_stop(false)
                .w_full()
                .max_w(px(600.0))
                .max_h(gpui::relative(1.0))
                .overflow_hidden()
                .flex()
                .flex_col()
                .rounded(px(14.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.surface)
                .text_color(theme.text)
                .shadow_xl()
                .child(
                    div()
                        .px(px(16.0))
                        .py(px(10.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_size(sp(15.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(
                            icon_button("close-service-model", "icons/x.svg", theme)
                                .tab_index(0)
                                .focus_visible(|style| style.bg(theme.overlay_strong))
                                .tooltip(Tooltip::text(cancel.clone()))
                                .on_activation(cx, |this, window, cx| {
                                    this.close_model_service_editor(window, cx)
                                }),
                        ),
                )
                .child(
                    div()
                        .id("service-model-editor-scroll")
                        .p(px(16.0))
                        .min_h_0()
                        .flex_1()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .children(
                            [
                                (
                                    format!("{} *", tr!("assistant.model")),
                                    TextField::new("service-model-id", form.model_id.clone()),
                                ),
                                (
                                    tr!("model_services.model_name"),
                                    TextField::new("service-model-name", form.model_name.clone()),
                                ),
                                (
                                    tr!("service_model.group"),
                                    TextField::new("service-model-group", form.model_group.clone()),
                                ),
                            ]
                            .map(|(label, field)| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(12.0))
                                    .child(
                                        div()
                                            .w(px(138.0))
                                            .flex_none()
                                            .text_size(sp(12.0))
                                            .text_color(theme.text_secondary)
                                            .child(label),
                                    )
                                    .child(field.min_w_0().flex_1())
                            }),
                        )
                        .child(render_model_options(form, cx))
                        .children(form.notice(NoticeAt::Catalog, theme)),
                )
                .child(
                    div()
                        .px(px(16.0))
                        .py(px(10.0))
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .child(assistant_button(
                            "cancel-service-model",
                            cancel,
                            cx,
                            |this, window, cx| {
                                this.close_model_service_editor(window, cx);
                            },
                        ))
                        .child(
                            model_service_save_button(action, true, theme)
                                .on_activation(cx, |this, window, cx| {
                                    this.finish_service_model(window, cx)
                                }),
                        ),
                ),
        )
    }

    fn render_model_service_catalog(&self, form: &ModelServiceForm, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let ready = form.work.is_none();
        let query = form.model_search.read(cx).content().trim().to_lowercase();
        let models = if query.is_empty() {
            form.models.clone()
        } else {
            Rc::new(
                form.models
                    .iter()
                    .filter(|model| {
                        model.id.to_lowercase().contains(&query)
                            || model.name.to_lowercase().contains(&query)
                            || model.options.group.to_lowercase().contains(&query)
                    })
                    .cloned()
                    .collect(),
            )
        };
        let model_count = models.len();
        let selected = form.selected.clone();
        let rows = gpui::uniform_list(
            "model-service-catalog",
            models.len(),
            cx.processor(move |_, range: Range<usize>, _, cx| {
                models[range]
                    .iter()
                    .map(|model| {
                        let id = model.id.clone();
                        let edit_id = id.clone();
                        let remove_id = id.clone();
                        let active = selected.as_deref() == Some(id.as_str());
                        div()
                            .h(px(34.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .when(active, |row| row.bg(theme.overlay_strong).rounded(px(6.0)))
                            .child(
                                assistant_button(
                                    SharedString::from(format!("pick-service-model-{id}")),
                                    [model.options.group.as_str(), model.display_name()]
                                        .into_iter()
                                        .filter(|piece| !piece.is_empty())
                                        .collect::<Vec<_>>()
                                        .join(" · "),
                                    cx,
                                    move |this, _, cx| {
                                        if let Some(form) = this.model_service_form.as_mut() {
                                            form.selected = Some(id.clone());
                                            cx.notify();
                                        }
                                    },
                                )
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden(),
                            )
                            .when(active, |row| {
                                row.child(icon("icons/check.svg", 13.0, theme.accent))
                            })
                            .when(ready, |row| {
                                row.child(assistant_button(
                                    SharedString::from(format!("edit-service-model-{edit_id}")),
                                    tr!("assistant.edit"),
                                    cx,
                                    move |this, window, cx| {
                                        this.edit_service_model(
                                            ModelEdit::Replace(edit_id.clone()),
                                            window,
                                            cx,
                                        )
                                    },
                                ))
                                .child(assistant_button(
                                    SharedString::from(format!("remove-service-model-{remove_id}")),
                                    tr!("model_services.remove_model"),
                                    cx,
                                    move |this, _, cx| this.remove_service_model(&remove_id, cx),
                                ))
                            })
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&form.scroll)
        .w_full()
        .h(px((model_count.clamp(1, 5) * 34) as f32));

        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr!("model_services.models")),
                    )
                    .when(ready, |row| {
                        row.child(
                            div()
                                .flex()
                                .gap(px(6.0))
                                .child(assistant_button(
                                    "sync-model-service",
                                    tr!("model_services.sync"),
                                    cx,
                                    |this, _, cx| this.request_model_service(Work::Sync, cx),
                                ))
                                .child(assistant_button(
                                    "add-service-model",
                                    tr!("model_services.add_model"),
                                    cx,
                                    |this, window, cx| {
                                        this.edit_service_model(ModelEdit::Add, window, cx)
                                    },
                                )),
                        )
                    }),
            )
            .when(!form.models.is_empty(), |section| {
                section.child(
                    TextField::new("search-service-models", form.model_search.clone())
                        .icon("icons/search.svg", 13.0),
                )
            })
            .when(model_count > 0, |section| section.child(rows))
            .when(model_count == 0 && !query.is_empty(), |section| {
                section.child(tr!("models.none_found"))
            })
    }

    fn close_model_service_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.model_service_form.as_mut() else {
            return;
        };
        if form.work == Some(Work::Save) {
            return;
        }
        if form.model_edit.is_some() {
            form.close_model_editor(window, cx);
            return;
        }
        self.model_service_form = None;
        let focus = if self.settings_page.is_some() {
            self.settings_focus.clone()
        } else {
            self.composer_focus(cx)
        };
        window.focus(&focus, cx);
        cx.notify();
    }

    fn edit_service_model(&mut self, edit: ModelEdit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self
            .model_service_form
            .as_mut()
            .filter(|form| form.work.is_none())
        else {
            return;
        };
        let model = match &edit {
            ModelEdit::Add => None,
            ModelEdit::Replace(id) => form.models.iter().find(|model| model.id == *id),
        };
        form.model_id.update(cx, |input, cx| {
            input.set_content(model.map_or("", |model| &model.id), cx)
        });
        form.model_name.update(cx, |input, cx| {
            input.set_content(model.map_or("", |model| &model.name), cx)
        });
        form.model_options =
            model
                .map(|model| model.options.clone())
                .unwrap_or_else(|| ServiceModelOptions {
                    protocol: form.catalog_protocol(form.url.read(cx).content()),
                    ..Default::default()
                });
        form.model_group.update(cx, |input, cx| {
            input.set_content(form.model_options.group.clone(), cx)
        });
        for (field, value) in [
            (&form.model_context, form.model_options.context_window),
            (
                &form.model_input_tokens,
                form.model_options.max_input_tokens,
            ),
            (
                &form.model_output_tokens,
                form.model_options.max_output_tokens,
            ),
        ] {
            field.update(cx, |input, cx| {
                input.set_content(value.map(|value| value.to_string()).unwrap_or_default(), cx)
            });
        }
        form.model_edit = Some(edit);
        form.status = None;
        let focus = form.model_id.read(cx).focus_handle(cx);
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
        cx.notify();
    }

    fn finish_service_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self
            .model_service_form
            .as_mut()
            .filter(|form| form.work.is_none())
        else {
            return;
        };
        let Some(edit) = form.model_edit.as_ref() else {
            return;
        };
        let id = form.model_id.read(cx).content().trim().to_owned();
        let name = form.model_name.read(cx).content().trim().to_owned();
        if id.is_empty()
            || form.models.iter().any(|model| {
                model.matches(&id)
                    && !matches!(edit, ModelEdit::Replace(previous) if *previous == model.id)
            })
        {
            form.status = Some((
                NoticeAt::Catalog,
                true,
                if id.is_empty() {
                    tr!("model_services.model_required")
                } else {
                    tr!("model_services.duplicate_model")
                },
            ));
            cx.notify();
            return;
        }
        let Some(options) = form.model_options_from_fields(cx) else {
            form.status = Some((NoticeAt::Catalog, true, tr!("service_model.invalid_tokens")));
            cx.notify();
            return;
        };
        let models = Rc::make_mut(&mut form.models);
        match edit {
            ModelEdit::Add => {
                let mut model = ServiceModel::new(id.clone());
                model.name = name;
                model.options = options;
                models.push(model);
                form.selected = Some(id);
            }
            ModelEdit::Replace(previous) => {
                if let Some(model) = models.iter_mut().find(|model| model.id == *previous) {
                    model.name = name;
                    model.options = options;
                    if model.id != id {
                        model.aliases.retain(|alias| alias != &id);
                        model
                            .aliases
                            .push(std::mem::replace(&mut model.id, id.clone()));
                        if form.selected.as_deref() == Some(previous) {
                            form.selected = Some(id);
                        }
                    }
                }
            }
        }
        models.sort_by(|a, b| a.options.group.cmp(&b.options.group).then(a.id.cmp(&b.id)));
        form.close_model_editor(window, cx);
    }

    fn remove_service_model(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(form) = self
            .model_service_form
            .as_mut()
            .filter(|form| form.work.is_none())
        else {
            return;
        };
        Rc::make_mut(&mut form.models).retain(|model| model.id != id);
        if form.selected.as_deref() == Some(id) {
            form.selected = None;
        }
        cx.notify();
    }

    fn request_model_service(&mut self, work: Work, cx: &mut Context<Self>) {
        let Some(form) = self
            .model_service_form
            .as_mut()
            .filter(|form| form.work.is_none())
        else {
            return;
        };
        let Some(base_url) = form.base_url(cx) else {
            form.status = Some((NoticeAt::Address, true, tr!("model_services.invalid_url")));
            cx.notify();
            return;
        };
        let api_key = form.key.read(cx).content().trim().to_owned();
        let protocol = form.catalog_protocol(&base_url);
        let command = if work == Work::Check {
            match form
                .selected
                .as_deref()
                .and_then(|id| form.models.iter().find(|model| model.id == id))
            {
                Some(model) => Command::TestModelService {
                    base_url,
                    api_key,
                    model: model.clone(),
                },
                None => Command::ListModelServiceModels {
                    base_url,
                    api_key,
                    protocol,
                },
            }
        } else {
            Command::ListModelServiceModels {
                base_url,
                api_key,
                protocol,
            }
        };
        let address = form.url.read(cx).content().to_owned();
        let generation = form.generation;
        let key_field = form.key.clone();
        let key_snapshot = key_field.read(cx).content().to_owned();
        let daemon = self.daemon.client();
        form.work = Some(work);
        form.status = None;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { daemon.request(Uuid::nil(), Uuid::nil(), command) })
                .await;
            let _ = view.update(cx, |this, cx| {
                let Some(form) = this
                    .model_service_form
                    .as_mut()
                    .filter(|form| form.generation == generation)
                else {
                    return;
                };
                form.work = None;
                if form.url.read(cx).content() == address
                    && key_field.read(cx).content() == key_snapshot
                {
                    form.status = Some(match result {
                        Ok(ResponsePayload::Ack) => {
                            (NoticeAt::Key, false, tr!("assistant.test_ok"))
                        }
                        Ok(ResponsePayload::ModelServiceModels { models })
                            if work == Work::Sync =>
                        {
                            let empty = models.is_empty();
                            merge_service_models(Rc::make_mut(&mut form.models), models, protocol);
                            if empty {
                                (NoticeAt::Catalog, true, tr!("model_services.sync_empty"))
                            } else {
                                (NoticeAt::Catalog, false, tr!("model_services.synced"))
                            }
                        }
                        Ok(ResponsePayload::ModelServiceModels { .. }) => {
                            (NoticeAt::Key, false, tr!("model_services.connection_ok"))
                        }
                        Ok(_) => (NoticeAt::Catalog, true, tr!("assistant.invalid_response")),
                        Err(error) => {
                            let message = error.to_string();
                            if message.contains("HTTP 401") || message.contains("HTTP 403") {
                                (NoticeAt::Key, true, tr!("model_services.key_rejected"))
                            } else {
                                (
                                    if work == Work::Check {
                                        NoticeAt::Key
                                    } else {
                                        NoticeAt::Catalog
                                    },
                                    true,
                                    message,
                                )
                            }
                        }
                    });
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_model_service(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self
            .model_service_form
            .as_mut()
            .filter(|form| form.work.is_none() && form.model_edit.is_none())
        else {
            return;
        };
        let Some(base_url) = form.base_url(cx) else {
            form.status = Some((NoticeAt::Address, true, tr!("model_services.invalid_url")));
            cx.notify();
            return;
        };
        let name = form.name.read(cx).content().trim().to_owned();
        if name.is_empty() {
            form.status = Some((NoticeAt::Catalog, true, tr!("model_services.name_required")));
            cx.notify();
            return;
        }
        let session = form.session;
        let selected = form.selected.clone();
        if session.is_some() && selected.is_none() {
            return;
        }
        let generation = form.generation;
        let id = form.original.as_ref().map_or(generation, |(id, _)| *id);
        let key = form.key.read(cx).content().trim().to_owned();
        let same_endpoint = form
            .original
            .as_ref()
            .is_some_and(|(_, url)| url == &base_url);
        let api_key = match form.original_key.as_ref() {
            Some(original) if same_endpoint && original.as_str() == key => None,
            Some(_) => Some(key),
            None => (!key.is_empty()).then_some(key),
        };
        let service = ModelService {
            id,
            name,
            base_url,
            enabled: form.enabled,
            models: (*form.models).clone(),
            legacy_key: false,
        };
        let daemon = self.daemon.client();
        let window_handle = window.window_handle();
        form.work = Some(Work::Save);
        form.status = None;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::SaveModelService { service, api_key },
                    )
                })
                .await;
            let focus = view
                .update(cx, |this, cx| {
                    let form = this
                        .model_service_form
                        .as_mut()
                        .filter(|form| form.generation == generation)?;
                    form.work = None;
                    match result {
                        Ok(ResponsePayload::ModelServiceSaved { service }) => {
                            if let Some(saved) = this
                                .state
                                .model_services
                                .iter_mut()
                                .find(|saved| saved.id == id)
                            {
                                *saved = service;
                            } else {
                                this.state.model_services.push(service);
                            }
                            this.model_service_form = None;
                            if let (Some(session), Some(model)) = (session, selected) {
                                if !this.use_assistant_service_model(session, id, model, cx) {
                                    this.show_toast(tr!("model_services.change_when_idle"));
                                }
                            }
                            cx.notify();
                            return Some(if this.settings_page.is_some() {
                                this.settings_focus.clone()
                            } else {
                                this.composer_focus(cx)
                            });
                        }
                        Ok(_) => {
                            form.status =
                                Some((NoticeAt::Catalog, true, tr!("assistant.invalid_response")))
                        }
                        Err(error) => {
                            form.status = Some((NoticeAt::Catalog, true, error.to_string()))
                        }
                    }
                    cx.notify();
                    None
                })
                .ok()
                .flatten();
            if let Some(focus) = focus {
                let _ = window_handle.update(cx, |_, window, cx| window.focus(&focus, cx));
            }
        })
        .detach();
    }
}

fn render_model_options(form: &ModelServiceForm, cx: &mut Context<Waku>) -> Div {
    let theme = Theme::current(cx);
    let options = &form.model_options;
    let dialogue = options.dialogue();
    let purpose = model_choices(
        "model-purpose",
        tr!("service_model.purpose"),
        [
            (ModelPurpose::Chat, tr!("service_model.chat")),
            (ModelPurpose::ImageGeneration, tr!("service_model.generate")),
            (ModelPurpose::ImageEdit, tr!("service_model.edit_image")),
        ],
        |value| options.purpose == value,
        cx,
        |options, value| {
            options.purpose = value;
            options.kind = if value == ModelPurpose::Chat {
                ModelKind::Text
            } else {
                ModelKind::Image
            };
        },
    );
    let kinds = model_choices(
        "model-kind",
        tr!("service_model.kind"),
        [
            (ModelKind::Text, tr!("service_model.text")),
            (ModelKind::Image, tr!("service_model.image")),
            (ModelKind::Embedding, tr!("service_model.embedding")),
            (ModelKind::Rerank, tr!("service_model.rerank")),
        ],
        |value| options.kind == value,
        cx,
        |options, value| {
            options.kind = value;
            options.purpose = if value == ModelKind::Image {
                if options.purpose == ModelPurpose::Chat
                    && !options.inputs.contains(&ModelModality::Vision)
                {
                    options.inputs.push(ModelModality::Vision);
                }
                options.purpose
            } else {
                ModelPurpose::Chat
            };
        },
    );
    let mut content = div().flex().flex_col().gap(px(12.0)).child(purpose);
    if dialogue {
        content = content.child(model_choices(
            "model-protocol",
            tr!("service_model.protocol"),
            [
                (ModelProtocol::OpenAiChat, "OpenAI Chat Completions".into()),
                (ModelProtocol::OpenAiResponses, "OpenAI Responses".into()),
                (
                    ModelProtocol::AnthropicMessages,
                    "Anthropic Messages".into(),
                ),
                (ModelProtocol::Gemini, "Gemini GenerateContent".into()),
            ],
            |value| options.protocol == value,
            cx,
            |options, value| {
                options.protocol = value;
                options.inputs.retain(|modality| value.supports(*modality));
            },
        ));
    }
    content = content.child(kinds);
    if dialogue {
        content = content.child(model_choices(
            "model-capability",
            tr!("service_model.capabilities"),
            [
                (true, tr!("service_model.reasoning")),
                (false, tr!("service_model.tools")),
            ],
            |reasoning| {
                if reasoning {
                    options.reasoning
                } else {
                    options.tools
                }
            },
            cx,
            |options, reasoning| {
                if reasoning {
                    options.reasoning = !options.reasoning;
                } else {
                    options.tools = !options.tools;
                }
            },
        ));
        let modalities = [
            (ModelModality::Vision, tr!("service_model.vision")),
            (ModelModality::Audio, tr!("service_model.audio")),
            (ModelModality::Video, tr!("service_model.video")),
        ]
        .into_iter()
        .filter(|(value, _)| options.protocol.supports(*value));
        content = content.child(model_choices(
            "model-input",
            tr!("service_model.inputs"),
            modalities,
            |value| options.inputs.contains(&value),
            cx,
            |options, value| {
                if options.inputs.contains(&value) {
                    options.inputs.retain(|item| *item != value);
                } else {
                    options.inputs.push(value);
                }
            },
        ));
    } else if options.purpose == ModelPurpose::ImageEdit {
        content = content.child(
            div()
                .text_size(sp(12.0))
                .text_color(theme.text_secondary)
                .child(tr!("service_model.edit_needs_image")),
        );
    } else if options.kind == ModelKind::Rerank {
        content = content.child(
            div()
                .text_size(sp(12.0))
                .text_color(theme.text_secondary)
                .child(tr!("service_model.rerank_format")),
        );
    }
    content
        .child(model_token_field(
            "model-context",
            tr!("service_model.context"),
            &form.model_context,
            &[128_000, 200_000, 256_000, 400_000, 512_000, 1_000_000],
            cx,
        ))
        .child(model_token_field(
            "model-max-input",
            tr!("service_model.max_input"),
            &form.model_input_tokens,
            &[128_000, 200_000, 256_000, 512_000, 1_000_000],
            cx,
        ))
        .when(dialogue, |form_el| {
            form_el.child(model_token_field(
                "model-max-output",
                tr!("service_model.max_output"),
                &form.model_output_tokens,
                &[16_000, 32_000, 64_000, 128_000, 256_000],
                cx,
            ))
        })
        .child(
            div()
                .text_size(sp(11.0))
                .text_color(theme.text_secondary)
                .child(tr!("service_model.token_hint")),
        )
}

fn model_choices<T: Copy + 'static>(
    id: &'static str,
    title: String,
    choices: impl IntoIterator<Item = (T, String)>,
    selected: impl Fn(T) -> bool,
    cx: &mut Context<Waku>,
    update: impl Fn(&mut ServiceModelOptions, T) + Copy + 'static,
) -> Div {
    let theme = Theme::current(cx);
    let buttons = choices.into_iter().enumerate().fold(
        div().flex().flex_wrap().gap(px(6.0)),
        |row, (index, (value, label))| {
            row.child(
                assistant_button((id, index), label, cx, move |this, _, cx| {
                    if let Some(form) = this.model_service_form.as_mut() {
                        update(&mut form.model_options, value);
                        form.status = None;
                        cx.notify();
                    }
                })
                .when(selected(value), |button| {
                    button.bg(theme.overlay_strong).border_color(theme.accent)
                }),
            )
        },
    );
    assistant_field(title, buttons, theme)
}

fn model_token_field(
    id: &'static str,
    label: String,
    field: &Entity<TextInput>,
    presets: &[u32],
    cx: &mut Context<Waku>,
) -> Div {
    let theme = Theme::current(cx);
    let buttons =
        presets
            .iter()
            .copied()
            .fold(div().flex().flex_wrap().gap(px(6.0)), |row, value| {
                let field = field.clone();
                let name = if value == 1_000_000 {
                    "1M".to_owned()
                } else {
                    format!("{}K", value / 1000)
                };
                row.child(assistant_button((id, value), name, cx, move |_, _, cx| {
                    field.update(cx, |input, cx| input.set_content(value.to_string(), cx));
                }))
            });
    assistant_field(
        label,
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(buttons)
            .child(TextField::new(id, field.clone())),
        theme,
    )
}

fn parse_token_limit(input: &str) -> Option<Option<u32>> {
    let text = input.trim();
    if text.is_empty() {
        Some(None)
    } else {
        text.parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .map(Some)
    }
}
fn model_service_save_button(label: String, enabled: bool, theme: Theme) -> Stateful<Div> {
    assistant_button_base("save-model-service", label, theme)
        .bg(theme.inverse)
        .text_color(theme.on_inverse)
        .when(!enabled, |button| button.opacity(0.45))
        .when(enabled, |button| button.hover(|style| style.opacity(0.85)))
}

fn model_service_dialog_layer(card: Stateful<Div>) -> AnyElement {
    gpui::deferred(
        div()
            .absolute()
            .inset_0()
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.35))
            .occlude()
            .child(card),
    )
    .with_priority(5)
    .into_any_element()
}

impl ModelServiceForm {
    fn model_options_from_fields(&self, cx: &App) -> Option<ServiceModelOptions> {
        let mut options = self.model_options.clone();
        options.group = self.model_group.read(cx).content().trim().to_owned();
        options.context_window = parse_token_limit(self.model_context.read(cx).content())?;
        options.max_input_tokens = parse_token_limit(self.model_input_tokens.read(cx).content())?;
        options.max_output_tokens = if options.dialogue() {
            parse_token_limit(self.model_output_tokens.read(cx).content())?
        } else {
            None
        };
        options.valid_token_limits().then_some(options)
    }

    /// Return to the supplier draft when the model dialog closes
    fn close_model_editor(&mut self, window: &mut Window, cx: &mut Context<Waku>) {
        self.model_edit = None;
        self.status = None;
        let focus = if self.models.is_empty() {
            &self.name
        } else {
            &self.model_search
        }
        .read(cx)
        .focus_handle(cx);
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
        cx.notify();
    }

    fn notice(&self, place: NoticeAt, theme: Theme) -> Option<Div> {
        let (at, failed, message) = self.status.as_ref()?;
        (*at == place).then(|| {
            div()
                .text_size(sp(12.0))
                .text_color(if *failed {
                    theme.warning
                } else {
                    theme.text_secondary
                })
                .child(message.clone())
        })
    }

    fn base_url(&self, cx: &App) -> Option<String> {
        let input = self.url.read(cx).content().trim();
        if let Some((_, original)) = self.original.as_ref().filter(|(_, url)| url == input) {
            return Some(original.clone());
        }
        model_service_base_url(input)
    }

    fn catalog_protocol(&self, address: &str) -> ModelProtocol {
        url::Url::parse(address.trim())
            .ok()
            .and_then(|url| ModelProtocol::from_host(url.host_str()?))
            .or_else(|| {
                self.selected
                    .as_deref()
                    .and_then(|id| self.models.iter().find(|model| model.id == id))
                    .or_else(|| self.models.first())
                    .map(|model| model.options.protocol)
            })
            .unwrap_or_default()
    }
}

/// Keep existing names, aliases and capabilities when the upstream catalog is refreshed
fn merge_service_models(
    existing: &mut Vec<ServiceModel>,
    incoming: Vec<String>,
    protocol: ModelProtocol,
) {
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
            let mut model = ServiceModel::new(id);
            model.options.protocol = protocol;
            model
        })
        .collect();
    existing.extend(new);
    existing.sort_by(|a, b| a.options.group.cmp(&b.options.group).then(a.id.cmp(&b.id)));
}

/// Accept an API root or a complete request endpoint
fn model_service_base_url(input: &str) -> Option<String> {
    let mut url = url::Url::parse(input).ok()?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.scheme(), "http" | "https")
    {
        return None;
    }
    if url.path() == "/" {
        url.set_path("/v1");
    }
    let cleaned = url.as_str().trim_end_matches('/');
    let suffixes = [
        "/chat/completions",
        "/responses",
        "/messages",
        "/images/generations",
        "/images/edits",
        "/embeddings",
        "/rerank",
    ];
    Some(
        suffixes
            .into_iter()
            .find_map(|suffix| cleaned.strip_suffix(suffix))
            .unwrap_or(cleaned)
            .to_owned(),
    )
}

/// Read saved credentials through the local keychain rather than exporting them over daemon RPC
#[cfg(target_os = "macos")]
fn read_local_key(service: &ModelService) -> anyhow::Result<Option<String>> {
    use waku_protocol::settings::MODEL_SERVICE_KEYCHAIN;
    let mut url = url::Url::parse(service.base_url.trim())?;
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    let account = service.keychain_account(url.as_str());
    for name in [
        Some(account.as_str()),
        service.legacy_key.then_some(url.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        match security_framework::passwords::get_generic_password(MODEL_SERVICE_KEYCHAIN, name) {
            Ok(key) => return Ok(Some(String::from_utf8(key)?)),
            Err(error) if error.code() == -25300 => {}
            Err(error) => return Err(error).context("Could not read the saved API key"),
        }
    }
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
fn read_local_key(_: &ModelService) -> anyhow::Result<Option<String>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_service_accepts_http_endpoint_on_the_network() {
        assert_eq!(
            model_service_base_url("http://192.168.1.50:8080/v1/chat/completions").as_deref(),
            Some("http://192.168.1.50:8080/v1")
        );
        assert_eq!(
            model_service_base_url("http://models.test:8080/v1/chat/completions").as_deref(),
            Some("http://models.test:8080/v1")
        );
    }

    #[test]
    fn model_service_save_button_builds_with_and_without_a_selection() {
        // GPUI rejects duplicate hover styles while constructing an enabled button
        for enabled in [false, true] {
            let _ = model_service_save_button("Add and use".into(), enabled, Theme::dark());
        }
    }

    #[test]
    fn model_service_sync_preserves_edited_models_and_manual_entries() {
        let mut models = vec![
            ServiceModel {
                id: "edited".into(),
                name: "My model".into(),
                aliases: vec!["original".into()],
                options: ServiceModelOptions::default(),
            },
            ServiceModel::new("manual".into()),
        ];
        merge_service_models(
            &mut models,
            vec!["original".into(), "new".into()],
            ModelProtocol::Gemini,
        );
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["edited", "manual", "new"]
        );
        assert_eq!(models[0].display_name(), "My model");
        assert_eq!(models[0].options.protocol, ModelProtocol::OpenAiChat);
        assert_eq!(models[2].options.protocol, ModelProtocol::Gemini);
    }
}
