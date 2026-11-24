//! Model service settings use the same full-height list and detail layout as Skills

use super::assistants::{assistant_button, assistant_button_base};
use super::composer::next_picker_highlight;
use super::settings::SETTINGS_SPLIT_LIST_WIDTH;
use super::*;
use crate::ui::ActivationExt as _;
use waku_client::{Command, ResponsePayload};
use waku_protocol::settings::ModelService;

const PAGE_CONTEXT: &str = "ModelServicesPane";
const SEARCH_CONTEXT: &str = "ModelServicesPane > TextInput";

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("down", SelectNextEntry, Some(SEARCH_CONTEXT)),
        gpui::KeyBinding::new("up", SelectPreviousEntry, Some(SEARCH_CONTEXT)),
    ]);
}

pub(super) struct ModelServicesPage {
    search: Entity<TextInput>,
    selected: Option<Uuid>,
    enabled_filter: Option<bool>,
    list_scroll: gpui::UniformListScrollHandle,
    pending: HashSet<Uuid>,
}

impl ModelServicesPage {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(window, cx)
                .clear_on_escape()
                .tab_stop()
                .placeholder(tr!("model_services.search_services"))
        });
        Self {
            search,
            selected: None,
            enabled_filter: None,
            list_scroll: gpui::UniformListScrollHandle::new(),
            pending: HashSet::new(),
        }
    }

    pub(super) fn subscribe(&self, cx: &mut Context<Waku>) {
        cx.subscribe(&self.search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                this.model_services_page.scroll_to_top();
                cx.notify();
            }
        })
        .detach();
    }

    pub(super) fn refresh_localized_text(&self, cx: &mut App) {
        self.search.update(cx, |input, cx| {
            input.set_placeholder(tr!("model_services.search_services"), cx);
        });
    }

    fn scroll_to_top(&self) {
        self.list_scroll
            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
    }

    pub(super) fn show_saved(&mut self, id: Uuid, creating: bool, cx: &mut App) {
        self.selected = Some(id);
        if creating {
            // Reveal the newly added service even if the old filter would hide it
            self.enabled_filter = None;
            self.search.update(cx, |input, cx| input.clear(cx));
        }
    }

    pub(super) fn is_saving(&self, id: Uuid) -> bool {
        self.pending.contains(&id)
    }

    fn selected_in(&self, visible: &[Uuid]) -> Option<Uuid> {
        self.selected
            .filter(|id| visible.contains(id))
            .or_else(|| visible.first().copied())
    }
}

impl Waku {
    fn visible_model_services(&self, cx: &App) -> Vec<Uuid> {
        let page = &self.model_services_page;
        let query = page.search.read(cx).content().trim().to_lowercase();
        self.state
            .model_services
            .iter()
            .filter(|service| {
                page.enabled_filter
                    .is_none_or(|enabled| enabled == service.enabled)
                    && (query.is_empty()
                        || service.name.to_lowercase().contains(&query)
                        || service.base_url.to_lowercase().contains(&query))
            })
            .map(|service| service.id)
            .collect()
    }

    fn step_model_service_selection(&mut self, key: &str, cx: &mut Context<Self>) {
        let visible = self.visible_model_services(cx);
        let page = &mut self.model_services_page;
        let current = page
            .selected_in(&visible)
            .and_then(|id| visible.iter().position(|entry| *entry == id));
        if let Some(next) = next_picker_highlight(current, visible.len(), key) {
            page.selected = Some(visible[next]);
            page.list_scroll
                .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    pub(super) fn apply_saved_model_service(&mut self, service: ModelService) {
        if let Some(saved) = self
            .state
            .model_services
            .iter_mut()
            .find(|saved| saved.id == service.id)
        {
            *saved = service;
        } else {
            self.state.model_services.push(service);
        }
    }

    fn set_model_service_enabled(&mut self, id: Uuid, enabled: bool, cx: &mut Context<Self>) {
        if !self
            .model_service(id)
            .is_some_and(|service| service.enabled != enabled)
            || !self.model_services_page.pending.insert(id)
        {
            return;
        }
        let daemon = self.daemon.client();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::SetModelServiceEnabled {
                            service_id: id,
                            enabled,
                        },
                    )
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.model_services_page.pending.remove(&id);
                match result {
                    Ok(ResponsePayload::ModelServiceSaved { service }) => {
                        this.apply_saved_model_service(service)
                    }
                    Ok(_) => this.show_toast(tr!("assistant.invalid_response")),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_model_services_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let visible = self.visible_model_services(cx);
        let selected = self.model_services_page.selected_in(&visible);
        let detail = selected.and_then(|id| self.model_service(id)).map_or_else(
            || {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(16.0))
                    .text_size(sp(12.5))
                    .text_color(theme.text_tertiary)
                    .child(tr!("model_services.select_placeholder"))
            },
            |service| self.render_model_service_detail(service, cx),
        );
        div()
            .size_full()
            .min_h_0()
            .flex()
            .child(self.render_model_service_list(visible, selected, cx))
            .child(div().flex_1().min_w_0().min_h_0().child(detail))
            .into_any_element()
    }

    fn render_model_service_list(
        &self,
        visible: Vec<Uuid>,
        selected: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let page = &self.model_services_page;
        let body = if visible.is_empty() {
            div()
                .px(px(16.0))
                .pt(px(12.0))
                .text_size(sp(12.5))
                .text_color(theme.text_secondary)
                .child(if self.state.model_services.is_empty() {
                    tr!("model_services.empty_services")
                } else {
                    tr!("model_services.no_matching_services")
                })
                .into_any_element()
        } else {
            gpui::uniform_list(
                "model-services-list",
                visible.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    visible[range]
                        .iter()
                        .filter_map(|id| this.model_service(*id))
                        .map(|service| {
                            Self::render_model_service_row(
                                service,
                                selected == Some(service.id),
                                cx,
                            )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&page.list_scroll)
            .size_full()
            .into_any_element()
        };
        let search = TextField::new("model-services-search", page.search.clone())
            .icon("icons/search.svg", 13.0)
            .w_full();
        let filter = self.render_model_services_filter(cx);
        let add = assistant_button(
            "add-model-service",
            tr!("model_services.add"),
            cx,
            |this, window, cx| this.open_model_service_editor(None, None, window, cx),
        )
        .w_full()
        .justify_center();

        div()
            .key_context(PAGE_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectNextEntry, _, cx| {
                this.step_model_service_selection("down", cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousEntry, _, cx| {
                this.step_model_service_selection("up", cx);
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
                    .flex()
                    .flex_col()
                    .gap(px(7.0))
                    .child(search)
                    .child(filter),
            )
            .child(div().flex_1().min_h_0().px(px(8.0)).child(body))
            .child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(add),
            )
    }

    fn render_model_services_filter(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let current = self.model_services_page.enabled_filter;
        let weak = cx.weak_entity();
        let handle = self.menu_handle("model-services-filter", cx);
        dropdown_menu(
            MenuChip::new("model-services-filter")
                .icon("icons/server.svg", theme.text_tertiary)
                .label(model_service_filter_label(current))
                .outlined()
                .background(theme.raised)
                .height(px(26.0))
                .selected(handle.is_open())
                .w_full()
                .justify_between(),
            "model-services-filter-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |_| {
                [None, Some(true), Some(false)]
                    .into_iter()
                    .map(|filter| {
                        let weak = weak.clone();
                        MenuItem::new(model_service_filter_label(filter), move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.model_services_page.enabled_filter = filter;
                                this.model_services_page.scroll_to_top();
                                cx.notify();
                            });
                        })
                        .selected(filter == current)
                    })
                    .collect()
            },
        )
    }

    fn render_model_service_row(
        service: &ModelService,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let id = service.id;
        div().h(px(56.0)).py(px(2.0)).w_full().child(
            div()
                .id(SharedString::from(format!("model-service-{id}")))
                .tab_index(0)
                .focus_visible(|row| row.border_1().border_color(theme.accent))
                .size_full()
                .px(px(9.0))
                .rounded(px(8.0))
                .cursor_default()
                .flex()
                .items_center()
                .gap(px(9.0))
                .when(selected, |row| row.bg(theme.sidebar_item_background))
                .when(!selected, |row| row.hover(|row| row.bg(theme.overlay)))
                .child(model_service_icon(theme, 26.0, 13.0, service.enabled))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(sp(12.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(if service.enabled {
                                            theme.text
                                        } else {
                                            theme.text_secondary
                                        })
                                        .child(service.name.clone()),
                                )
                                .when(!service.enabled, |name| {
                                    name.child(
                                        div()
                                            .flex_none()
                                            .text_size(sp(11.0))
                                            .text_color(theme.text_tertiary)
                                            .child(model_service_filter_label(Some(false))),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(sp(12.5))
                                .text_color(theme.text_tertiary)
                                .child(service.base_url.clone()),
                        ),
                )
                .on_activation(cx, move |this, _, cx| {
                    this.model_services_page.selected = Some(id);
                    cx.notify();
                }),
        )
    }

    fn render_model_service_detail(&self, service: &ModelService, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let id = service.id;
        let enabled = service.enabled;
        let pending = self.model_services_page.is_saving(id);
        let toggle = toggle_switch(
            "model-service-detail-enabled",
            enabled,
            pending,
            theme,
            cx,
            move |this, _, cx| this.set_model_service_enabled(id, !enabled, cx),
        )
        .tooltip(Tooltip::text(tr!("model_services.enabled")));
        let edit = assistant_button_base(
            "model-service-detail-edit",
            tr!("model_services.edit"),
            theme,
        )
        .tab_stop(!pending)
        .when(pending, |button| button.opacity(0.55))
        .when(!pending, |button| {
            button
                .hover(|style| style.bg(theme.overlay))
                .on_activation(cx, move |this, window, cx| {
                    this.open_model_service_editor(Some(id), None, window, cx)
                })
        });
        let models = if service.models.is_empty() {
            div()
                .text_size(sp(12.5))
                .text_color(theme.text_secondary)
                .child(tr!("model_services.empty_catalog"))
                .into_any_element()
        } else {
            gpui::uniform_list(
                SharedString::from(format!("model-service-detail-models-{id}")),
                service.models.len(),
                cx.processor(move |this, range: Range<usize>, _, _| {
                    let Some(service) = this.model_service(id) else {
                        return Vec::new();
                    };
                    range
                        .filter_map(|index| service.models.get(index))
                        .map(|model| {
                            div()
                                .h(px(36.0))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(sp(12.5))
                                        .text_color(theme.text)
                                        .child(model.display_name().to_owned()),
                                )
                                .when(!model.options.group.is_empty(), |row| {
                                    row.child(
                                        div()
                                            .max_w(gpui::relative(0.4))
                                            .truncate()
                                            .text_size(sp(12.0))
                                            .text_color(theme.text_tertiary)
                                            .child(model.options.group.clone()),
                                    )
                                })
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .size_full()
            .into_any_element()
        };

        div()
            .size_full()
            .min_h_0()
            .min_w_0()
            .px(px(24.0))
            .pt(px(18.0))
            .pb(px(20.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(model_service_icon(theme, 38.0, 18.0, enabled))
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
                                    .child(service.name.clone()),
                            )
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .text_size(sp(12.5))
                                    .text_color(theme.text_tertiary)
                                    .child(if pending {
                                        tr!("model_services.working")
                                    } else {
                                        model_service_filter_label(Some(enabled))
                                    }),
                            ),
                    )
                    .child(toggle),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .py(px(8.0))
                    .flex_none()
                    .flex()
                    .items_baseline()
                    .gap(px(12.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .w(px(84.0))
                            .flex_none()
                            .text_size(sp(12.5))
                            .text_color(theme.text_tertiary)
                            .child(tr!("model_services.endpoint")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(crate::md::render::MONO_FAMILY)
                            .text_size(sp(12.0))
                            .text_color(theme.text_secondary)
                            .child(service.base_url.clone()),
                    ),
            )
            .child(
                div()
                    .mt(px(12.0))
                    .pb(px(14.0))
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(edit),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pt(px(14.0))
                    .pb(px(6.0))
                    .child(
                        div()
                            .text_size(sp(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("model_services.models")),
                    )
                    .child(
                        div()
                            .text_size(sp(12.0))
                            .text_color(theme.text_tertiary)
                            .child(service.models.len().to_string()),
                    ),
            )
            .child(div().flex_1().min_h_0().child(models))
    }
}

fn model_service_filter_label(enabled: Option<bool>) -> String {
    match enabled {
        None => tr!("model_services.all_services"),
        Some(true) => tr!("model_services.enabled_status"),
        Some(false) => tr!("model_services.disabled_status"),
    }
}

fn model_service_icon(theme: Theme, size: f32, icon_size: f32, enabled: bool) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(size * 0.24))
        .bg(theme.overlay)
        .flex()
        .items_center()
        .justify_center()
        .child(icon(
            "icons/server.svg",
            icon_size,
            theme
                .text_secondary
                .opacity(if enabled { 1.0 } else { 0.45 }),
        ))
}
