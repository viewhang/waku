//! Model service settings use the same full-height list and detail layout as Skills

use super::assistants::{assistant_button, assistant_button_base};
use super::composer::next_picker_highlight;
use super::model_capabilities::{ModelCategory, model_capability_badges};
use super::model_services::CatalogAction;
use super::settings::SETTINGS_SPLIT_LIST_WIDTH;
use super::*;
use crate::ui::ActivationExt as _;
use waku_client::{Command, ResponsePayload};
use waku_protocol::settings::{ModelService, ServiceModel};

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
    model_search: Entity<TextInput>,
    selected: Option<Uuid>,
    enabled_filter: Option<bool>,
    list_scroll: gpui::UniformListScrollHandle,
    model_scroll: gpui::UniformListScrollHandle,
    pending: HashSet<Uuid>,
    model_filter: Option<(Uuid, ModelCategory)>,
    batch: Option<(Uuid, HashSet<String>)>,
    catalog: RefCell<Option<(Uuid, ModelCategory, String, Rc<CatalogIndex>)>>,
}

enum CatalogRow {
    Group(SharedString),
    Model(usize),
}

struct CatalogIndex {
    counts: [usize; ModelCategory::ALL.len()],
    rows: Vec<CatalogRow>,
}

impl CatalogIndex {
    fn new(models: &[ServiceModel], filter: ModelCategory, query: &str) -> Self {
        let mut counts = [0; ModelCategory::ALL.len()];
        let mut group_indices = HashMap::new();
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (index, model) in models.iter().enumerate() {
            if !query.is_empty()
                && ![
                    model.id.as_str(),
                    model.display_name(),
                    &model.options.group,
                ]
                .iter()
                .any(|value| value.to_lowercase().contains(query))
            {
                continue;
            }
            for category in ModelCategory::ALL {
                counts[category.index()] += usize::from(category.matches(&model.options));
            }
            if filter.matches(&model.options) {
                let group = model.options.group.trim();
                let group_index = *group_indices.entry(group).or_insert_with(|| {
                    groups.push((group, Vec::new()));
                    groups.len() - 1
                });
                groups[group_index].1.push(index);
            }
        }
        let grouped = groups.iter().any(|(name, _)| !name.is_empty());
        let mut rows = Vec::new();
        for (name, members) in groups {
            if grouped {
                rows.push(CatalogRow::Group(name.to_owned().into()));
            }
            rows.extend(members.into_iter().map(CatalogRow::Model));
        }
        Self { counts, rows }
    }

    /// Collect only the models included in the current search and category
    fn model_ids(&self, models: &[ServiceModel]) -> HashSet<String> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                CatalogRow::Model(index) => models.get(*index).map(|model| model.id.clone()),
                CatalogRow::Group(_) => None,
            })
            .collect()
    }
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
            model_search: cx.new(|cx| {
                TextInput::new(window, cx)
                    .clear_on_escape()
                    .tab_stop()
                    .placeholder(tr!("model_services.search_models"))
            }),
            selected: None,
            enabled_filter: None,
            list_scroll: gpui::UniformListScrollHandle::new(),
            model_scroll: gpui::UniformListScrollHandle::new(),
            pending: HashSet::new(),
            model_filter: None,
            batch: None,
            catalog: RefCell::new(None),
        }
    }

    pub(super) fn subscribe(&self, cx: &mut Context<Waku>) {
        cx.subscribe(&self.search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                this.model_services_page.batch = None;
                this.model_services_page.scroll_to_top();
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&self.model_search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                if let Some((_, models)) = &mut this.model_services_page.batch {
                    models.clear();
                }
                this.model_services_page.scroll_models_to_top();
                cx.notify();
            }
        })
        .detach();
    }

    pub(super) fn refresh_localized_text(&self, cx: &mut App) {
        self.search.update(cx, |input, cx| {
            input.set_placeholder(tr!("model_services.search_services"), cx);
        });
        self.model_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("model_services.search_models"), cx);
        });
    }

    fn scroll_to_top(&self) {
        self.list_scroll
            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
    }

    fn scroll_models_to_top(&self) {
        self.model_scroll
            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
    }

    fn select_service(&mut self, id: Uuid) {
        if self
            .batch
            .as_ref()
            .is_some_and(|(service, _)| *service != id)
        {
            self.batch = None;
        }
        self.selected = Some(id);
        self.scroll_models_to_top();
    }

    fn batch_models(&self, id: Uuid) -> Option<&HashSet<String>> {
        self.batch
            .as_ref()
            .filter(|(service, _)| *service == id)
            .map(|(_, ids)| ids)
    }

    pub(super) fn show_saved(&mut self, id: Uuid, creating: bool, cx: &mut App) {
        self.selected = Some(id);
        if creating {
            // Reveal the newly added service even if the old filter would hide it
            self.enabled_filter = None;
            self.search.update(cx, |input, cx| input.clear(cx));
            self.model_search.update(cx, |input, cx| input.clear(cx));
        }
    }

    /// Clear search and category filters that could hide the newly added model
    pub(super) fn reveal_added_model(&mut self, cx: &mut App) {
        self.model_filter = None;
        self.model_search.update(cx, |input, cx| input.clear(cx));
        self.scroll_models_to_top();
    }

    pub(super) fn is_saving(&self, id: Uuid) -> bool {
        self.pending.contains(&id)
    }

    fn selected_in(&self, visible: &[Uuid]) -> Option<Uuid> {
        self.selected
            .filter(|id| visible.contains(id))
            .or_else(|| visible.first().copied())
    }

    fn model_filter(&self, service_id: Uuid) -> ModelCategory {
        self.model_filter
            .filter(|(id, _)| *id == service_id)
            .map(|(_, category)| category)
            .unwrap_or_default()
    }

    /// Reuse counts and grouped rows between frames until the provider or filter changes
    fn catalog(
        &self,
        service: &ModelService,
        filter: ModelCategory,
        query: &str,
    ) -> Rc<CatalogIndex> {
        let mut cached = self.catalog.borrow_mut();
        if let Some((id, current, cached_query, index)) = cached.as_ref()
            && *id == service.id
            && *current == filter
            && cached_query == query
        {
            return index.clone();
        }
        let index = Rc::new(CatalogIndex::new(&service.models, filter, query));
        *cached = Some((service.id, filter, query.to_owned(), index.clone()));
        index
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
            page.select_service(visible[next]);
            page.list_scroll
                .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    pub(super) fn apply_saved_model_service(&mut self, service: ModelService) {
        self.model_services_page.catalog.get_mut().take();
        if self.model_services_page.batch_models(service.id).is_some() {
            self.model_services_page.batch = None;
        }
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

    fn model_service_catalog(
        &self,
        service: &ModelService,
        cx: &App,
    ) -> (ModelCategory, Rc<CatalogIndex>) {
        let page = &self.model_services_page;
        let filter = page.model_filter(service.id);
        let query = page.model_search.read(cx).content().trim().to_lowercase();
        (filter, page.catalog(service, filter, &query))
    }

    fn toggle_all_model_results(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(service) = self.model_service(id) else {
            return;
        };
        let (_, catalog) = self.model_service_catalog(service, cx);
        let visible = catalog.model_ids(&service.models);
        if let Some((service, selected)) = &mut self.model_services_page.batch
            && *service == id
        {
            if *selected == visible {
                selected.clear();
            } else {
                *selected = visible;
            }
            cx.notify();
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
                    Ok(_) => this.show_error_toast(tr!("assistant.invalid_response")),
                    Err(error) => this.show_error_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_model_service_catalog(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.model_services_page.pending.insert(id) {
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
                        Command::SyncModelService { service_id: id },
                    )
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.model_services_page.pending.remove(&id);
                match result {
                    Ok(ResponsePayload::ModelServiceSaved { service }) => {
                        this.apply_saved_model_service(service);
                        this.show_success_toast(tr!("model_services.synced"));
                    }
                    Ok(ResponsePayload::ModelServiceModels { .. }) => {
                        this.show_warning_toast(tr!("model_services.sync_empty"));
                    }
                    Ok(_) => this.show_error_toast(tr!("assistant.invalid_response")),
                    Err(error) => this.show_error_toast(error.to_string()),
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
                                this.model_services_page.batch = None;
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
                    this.model_services_page.select_service(id);
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
        let edit_label = tr!("model_services.edit");
        let edit = icon_button("model-service-detail-edit", "icons/settings.svg", theme)
            .size(px(26.0))
            .tab_index(0)
            .tab_stop(!pending)
            .focus_visible(|button| button.bg(theme.overlay_strong))
            .aria_label(edit_label.clone())
            .tooltip(Tooltip::text(edit_label))
            .when(pending, |button| button.opacity(0.55))
            .on_activation(cx, move |this, window, cx| {
                this.open_model_service_editor(Some(id), None, window, cx)
            });
        let (filter, catalog) = self.model_service_catalog(service, cx);
        let visible_count = catalog.counts[filter.index()];
        let filter_bar = (!service.models.is_empty())
            .then(|| Self::render_model_categories(id, filter, &catalog, cx));
        let models = Self::render_catalog_rows(
            service,
            filter,
            catalog,
            &self.model_services_page.model_scroll,
            cx,
        );

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
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(sp(15.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(service.name.clone()),
                                    )
                                    .child(edit.flex_none()),
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
            .child(self.render_model_catalog_toolbar(service, pending, visible_count, cx))
            .children(filter_bar)
            .child(div().w_full().flex_1().min_h_0().child(models))
    }

    fn render_model_catalog_toolbar(
        &self,
        service: &ModelService,
        pending: bool,
        visible_count: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let id = service.id;
        let bar = div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .pt(px(16.0))
            .pb(px(10.0))
            .child(
                div()
                    .flex_none()
                    .text_size(sp(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("model_services.models")),
            )
            .child(div().flex_1())
            .when(!service.models.is_empty(), |bar| {
                bar.child(
                    TextField::new(
                        "model-catalog-search",
                        self.model_services_page.model_search.clone(),
                    )
                    .icon("icons/search.svg", 13.0)
                    .min_w_0()
                    .max_w(px(230.0))
                    .flex_1(),
                )
            });
        if let Some(selected) = self.model_services_page.batch_models(id) {
            let count = selected.len();
            let can_select = !pending && visible_count > 0;
            let can_remove = !pending && count > 0;
            return bar
                .child(catalog_toolbar_button(
                    "toggle-all-model-results",
                    if can_select && count == visible_count {
                        tr!("model_services.clear_selection")
                    } else {
                        tr!("model_services.select_all_results")
                    },
                    can_select,
                    theme,
                    cx,
                    move |this, _, cx| this.toggle_all_model_results(id, cx),
                ))
                .child(
                    assistant_button(
                        "cancel-batch-model-removal",
                        tr!("model_services.cancel"),
                        cx,
                        |this, _, cx| {
                            this.model_services_page.batch = None;
                            cx.notify();
                        },
                    )
                    .flex_none(),
                )
                .child(
                    catalog_toolbar_button(
                        "remove-selected-models",
                        tr!("model_services.remove_selected", count = count),
                        can_remove,
                        theme,
                        cx,
                        move |this, window, cx| {
                            if let Some(ids) = this.model_services_page.batch_models(id).cloned() {
                                this.open_catalog_model(id, CatalogAction::Remove(ids), window, cx);
                            }
                        },
                    )
                    .text_color(theme.danger),
                );
        }
        bar.when(!service.models.is_empty(), |bar| {
            bar.child(catalog_toolbar_button(
                "batch-remove-models",
                tr!("model_services.batch_remove"),
                !pending && visible_count > 0,
                theme,
                cx,
                move |this, _, cx| {
                    if !this.model_services_page.is_saving(id) {
                        this.model_services_page.batch = Some((id, HashSet::new()));
                        cx.notify();
                    }
                },
            ))
        })
        .child(catalog_toolbar_button(
            "sync-saved-models",
            tr!("model_services.sync"),
            !pending,
            theme,
            cx,
            move |this, _, cx| this.sync_model_service_catalog(id, cx),
        ))
        .child(catalog_toolbar_button(
            "add-saved-model",
            tr!("model_services.add_model"),
            !pending,
            theme,
            cx,
            move |this, window, cx| this.open_catalog_model(id, CatalogAction::Add, window, cx),
        ))
    }

    fn render_model_categories(
        service_id: Uuid,
        filter: ModelCategory,
        catalog: &CatalogIndex,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        div()
            .tab_group()
            .tab_stop(false)
            .w_full()
            .flex()
            .flex_none()
            .flex_wrap()
            .justify_between()
            .gap(px(6.0))
            .mb(px(12.0))
            .p(px(5.0))
            .rounded(px(8.0))
            .bg(theme.inset)
            .children(ModelCategory::ALL.map(|category| {
                let selected = filter == category;
                div()
                    .id(("model-category", category.index()))
                    .tab_index(0)
                    .focus_visible(|style| style.border_color(theme.accent))
                    .h(px(30.0))
                    .px(px(9.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(if selected {
                        theme.border_strong
                    } else {
                        gpui::transparent_black()
                    })
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(sp(12.0))
                    .font_weight(if selected {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_secondary
                    })
                    .when(selected, |chip| chip.bg(theme.raised))
                    .when(!selected, |chip| {
                        chip.hover(|style| style.bg(theme.overlay))
                    })
                    .child(icon(category.icon(), 12.0, theme.text_secondary))
                    .child(category.label())
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .child(catalog.counts[category.index()].to_string()),
                    )
                    .on_activation(cx, move |this, _, cx| {
                        if this.model_services_page.model_filter(service_id) != category
                            && let Some((id, selected)) = &mut this.model_services_page.batch
                            && *id == service_id
                        {
                            selected.clear();
                        }
                        this.model_services_page.model_filter = Some((service_id, category));
                        this.model_services_page.scroll_models_to_top();
                        cx.notify();
                    })
            }))
    }

    fn render_catalog_rows(
        service: &ModelService,
        filter: ModelCategory,
        catalog: Rc<CatalogIndex>,
        scroll: &gpui::UniformListScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        if catalog.rows.is_empty() {
            return div()
                .px(px(8.0))
                .pt(px(7.0))
                .text_size(sp(12.5))
                .text_color(theme.text_secondary)
                .child(if service.models.is_empty() {
                    tr!("model_services.empty_catalog")
                } else {
                    tr!("models.none_found")
                })
                .into_any_element();
        }
        let id = service.id;
        gpui::uniform_list(
            SharedString::from(format!(
                "model-service-detail-models-{id}-{}",
                filter.index()
            )),
            catalog.rows.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let Some(service) = this.model_service(id) else {
                    return Vec::new();
                };
                let pending = this.model_services_page.is_saving(id);
                let selected = this.model_services_page.batch_models(id);
                catalog.rows[range]
                    .iter()
                    .filter_map(|row| match row {
                        CatalogRow::Group(group) => Some(
                            div()
                                .w_full()
                                .h(px(44.0))
                                .px(px(12.0))
                                .bg(theme.inset)
                                .flex()
                                .items_center()
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(sp(12.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.text_secondary)
                                        .child(if group.is_empty() {
                                            tr!("service_model.ungrouped").into()
                                        } else {
                                            group.clone()
                                        }),
                                )
                                .into_any_element(),
                        ),
                        CatalogRow::Model(index) => service.models.get(*index).map(|model| {
                            let checked = selected.map(|ids| ids.contains(&model.id));
                            Self::render_catalog_model(id, model, checked, pending, theme, cx)
                                .into_any_element()
                        }),
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(scroll)
        .size_full()
        .into_any_element()
    }

    fn render_catalog_model(
        service_id: Uuid,
        model: &ServiceModel,
        checked: Option<bool>,
        pending: bool,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let row = div()
            .id(SharedString::from(model.id.clone()))
            // UniformList gives each row available width, which the row must explicitly fill
            .w_full()
            .h(px(44.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .border_b_1()
            .border_color(theme.border)
            .child(if let Some(checked) = checked {
                div()
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .size(px(16.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if checked {
                                theme.inverse
                            } else {
                                theme.border_strong
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(checked, |checkbox| {
                                checkbox.bg(theme.inverse).child(icon(
                                    "icons/check.svg",
                                    12.0,
                                    theme.on_inverse,
                                ))
                            }),
                    )
            } else {
                div()
                    .size(px(24.0))
                    .rounded_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme.overlay)
                    .child(icon("icons/bot.svg", 14.0, theme.text_secondary))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(sp(13.0))
                    .text_color(theme.text)
                    .child(model.display_name().to_owned()),
            )
            .child(model_capability_badges(&model.options, theme));
        if let Some(checked) = checked {
            let id = model.id.clone();
            return row
                .aria_label(model.display_name().to_owned())
                .aria_selected(checked)
                .tab_index(0)
                .tab_stop(!pending)
                .focus_visible(|row| row.border_1().border_color(theme.accent))
                .when(checked, |row| row.bg(theme.sidebar_item_background))
                .hover(|row| row.bg(theme.overlay))
                .when(!pending, |row| {
                    row.on_activation(cx, move |this, _, cx| {
                        if let Some((service, selected)) = &mut this.model_services_page.batch
                            && *service == service_id
                        {
                            if !selected.remove(&id) {
                                selected.insert(id.clone());
                            }
                            cx.notify();
                        }
                    })
                });
        }
        let edit_id = model.id.clone();
        let remove_id = model.id.clone();
        let edit = tr!("model_services.edit_model");
        let remove = tr!("model_services.remove_model");
        row.child(
            div()
                .ml(px(4.0))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.0))
                .child(
                    icon_button("edit-model", "icons/settings.svg", theme)
                        .size(px(26.0))
                        .tab_index(0)
                        .tab_stop(!pending)
                        .focus_visible(|button| button.bg(theme.overlay_strong))
                        .aria_label(edit.clone())
                        .tooltip(Tooltip::text(edit))
                        .on_activation(cx, move |this, window, cx| {
                            this.open_catalog_model(
                                service_id,
                                CatalogAction::Edit(edit_id.clone()),
                                window,
                                cx,
                            );
                        }),
                )
                .child(
                    icon_button("remove-model", "icons/window-minimize.svg", theme)
                        .size(px(26.0))
                        .tab_index(0)
                        .tab_stop(!pending)
                        .focus_visible(|button| button.bg(theme.overlay_strong))
                        .aria_label(remove.clone())
                        .tooltip(Tooltip::text(remove))
                        .on_activation(cx, move |this, window, cx| {
                            this.open_catalog_model(
                                service_id,
                                CatalogAction::Remove(HashSet::from([remove_id.clone()])),
                                window,
                                cx,
                            );
                        }),
                ),
        )
    }
}

fn catalog_toolbar_button(
    id: &'static str,
    label: String,
    enabled: bool,
    theme: Theme,
    cx: &mut Context<Waku>,
    action: impl Fn(&mut Waku, &mut Window, &mut Context<Waku>) + 'static,
) -> Stateful<Div> {
    assistant_button_base(id, label, theme)
        .flex_none()
        .tab_stop(enabled)
        .when(!enabled, |button| button.opacity(0.55))
        .when(enabled, |button| {
            button
                .hover(|style| style.bg(theme.overlay))
                .on_activation(cx, action)
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use waku_protocol::settings::{ModelKind, ModelModality, ModelProtocol, ModelPurpose};

    #[test]
    fn catalog_filters_by_configured_type_and_supported_inputs() {
        let mut vision = ServiceModel::new("vision-chat".into());
        vision.options.group = "Chat".into();
        vision.options.inputs = vec![ModelModality::Vision, ModelModality::Video];
        let mut multimodal = ServiceModel::new("multimodal-chat".into());
        multimodal.options.group = "Chat".into();
        multimodal.options.protocol = ModelProtocol::Gemini;
        multimodal.options.inputs = vec![ModelModality::Audio, ModelModality::Video];
        let mut image = ServiceModel::new("image-editor".into());
        image.options.group = "Images".into();
        image.options.kind = ModelKind::Image;
        image.options.purpose = ModelPurpose::ImageEdit;
        let mut speech = ServiceModel::discovered("tts".into(), ModelProtocol::OpenAiChat);
        speech.options.kind = ModelKind::Speech;
        let mut transcription =
            ServiceModel::discovered("transcribe".into(), ModelProtocol::OpenAiChat);
        transcription.options.kind = ModelKind::Transcription;
        let unknown = ServiceModel::discovered("unknown".into(), ModelProtocol::OpenAiChat);
        let mut moderation =
            ServiceModel::discovered("moderation".into(), ModelProtocol::OpenAiChat);
        moderation.options.kind = ModelKind::Moderation;
        let models = [
            vision,
            image,
            multimodal,
            speech,
            transcription,
            unknown,
            moderation,
        ];

        let all = CatalogIndex::new(&models, ModelCategory::All, "");
        assert_eq!(all.counts, [7, 2, 1, 0, 1, 1, 0, 1, 1, 2]);
        assert!(
            matches!(all.rows.as_slice(), [CatalogRow::Group(chat), CatalogRow::Model(0), CatalogRow::Model(2), CatalogRow::Group(images), CatalogRow::Model(1), ..]
            if chat.as_ref() == "Chat" && images.as_ref() == "Images")
        );
        let images = CatalogIndex::new(&models, ModelCategory::Image, "");
        assert_eq!(images.counts, all.counts);
        assert!(matches!(
            images.rows.as_slice(),
            [CatalogRow::Group(_), CatalogRow::Model(1)]
        ));
        let video = CatalogIndex::new(&models, ModelCategory::Video, "");
        assert!(matches!(
            video.rows.as_slice(),
            [CatalogRow::Group(_), CatalogRow::Model(2)]
        ));
        let other = CatalogIndex::new(&models, ModelCategory::Other, "");
        assert!(matches!(
            other.rows.as_slice(),
            [CatalogRow::Model(5), CatalogRow::Model(6)]
        ));
        let searched = CatalogIndex::new(&models, ModelCategory::Text, "vision");
        assert_eq!(searched.counts, [1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            searched.model_ids(&models),
            HashSet::from(["vision-chat".into()])
        );
        assert_eq!(
            other.model_ids(&models),
            HashSet::from(["unknown".into(), "moderation".into()])
        );
        assert!(matches!(
            searched.rows.as_slice(),
            [CatalogRow::Group(_), CatalogRow::Model(0)]
        ));
    }
}
