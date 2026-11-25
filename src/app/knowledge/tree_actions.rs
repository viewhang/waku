//! The row menu, keyboard and drag targets use the same library-scoped tree commands

use super::*;
use row_actions::document_parts;

/// A delayed menu or drag must still refer to the same library and document
#[derive(Clone, PartialEq)]
pub(super) struct TreeItem {
    pub(super) root: String,
    pub(super) path: String,
    pub(super) id: Option<Uuid>,
}

impl TreeItem {
    pub(super) fn new(row: &TreeRow, root: &str) -> Self {
        Self {
            root: root.into(),
            path: row.path.clone(),
            id: row.id,
        }
    }

    pub(super) fn operation(&self, action: LibraryAction) -> KnowledgeOperation {
        KnowledgeOperation::ManageTree {
            root: self.root.clone(),
            id: self.id,
            action,
        }
    }

    /// Component boundaries distinguish a descendant from a sibling with the same name prefix
    fn accepts_parent(&self, parent: &str) -> bool {
        self.id.is_some() || !Path::new(parent).starts_with(&self.path)
    }
}

impl Render for TreeItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        div()
            .px(px(8.))
            .py(px(3.))
            .rounded(px(5.))
            .bg(theme.surface)
            .text_color(theme.text)
            .text_size(sp(12.5))
            .child(document_parts(&self.path).1.to_owned())
    }
}

/// Cache available destinations when the picker opens so rendering touches only visible folders
pub(super) struct MovePicker {
    item: TreeItem,
    folders: Vec<String>,
    cursor: usize,
    focus: FocusHandle,
    scroll: gpui::UniformListScrollHandle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DropZone {
    Before,
    Inside,
    After,
}

/// File rows split in half while folders reserve their center for moving inside
fn drop_zone(folder: bool, offset: gpui::Pixels, height: gpui::Pixels) -> DropZone {
    if folder && offset >= height * 0.25 && offset < height * 0.75 {
        DropZone::Inside
    } else if offset < height * 0.5 {
        DropZone::Before
    } else {
        DropZone::After
    }
}

pub(super) fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    }
}

impl KnowledgePage {
    /// Preserve the visible document and expansion state until the refreshed catalog arrives
    pub(super) fn follow_move(&mut self, from: &str, to: &str) {
        self.reveal_after_move = Some(to.into());
        if from == to {
            return;
        }
        let remap = |path: &str| {
            if path == from {
                return to.into();
            }
            if let Some(suffix) = path.strip_prefix(from).filter(|tail| tail.starts_with('/')) {
                return format!("{to}{suffix}");
            }
            path.to_owned()
        };
        self.collapsed = self.collapsed.drain().map(|p| remap(&p)).collect();
        if let Some(loc) = self.location.as_mut() {
            loc.path = remap(&loc.path);
        }
    }
}

impl Waku {
    pub(super) fn has_tree_item(&self, item: &TreeItem) -> bool {
        self.knowledge
            .status
            .config
            .as_ref()
            .is_some_and(|cfg| cfg.root == item.root)
            && self
                .knowledge
                .tree
                .iter()
                .any(|r| r.path == item.path && r.id == item.id)
    }

    pub(super) fn tree_editable(&mut self, cx: &mut Context<Self>) -> bool {
        !self.knowledge.busy
            && !self.knowledge.changing_libraries
            && self.knowledge.rename_save.is_none()
            && !self.has_library_draft(cx)
    }

    /// Skip descendants and keep desktop-pinned notes in their leading section
    pub(super) fn tree_neighbor(&self, item: &TreeItem, after: bool) -> Option<String> {
        let parent = document_parts(&item.path).0;
        let pinned =
            |id: Option<Uuid>| id.is_some_and(|id| self.state.pinned_documents.contains(&id));
        let mut siblings = self
            .knowledge
            .tree
            .iter()
            .filter(|r| document_parts(&r.path).0 == parent && pinned(r.id) == pinned(item.id));
        if after {
            siblings.find(|r| r.path == item.path)?;
            siblings.next().map(|r| r.path.clone())
        } else {
            siblings
                .take_while(|r| r.path != item.path)
                .last()
                .map(|r| r.path.clone())
        }
    }

    pub(super) fn move_tree_item(&mut self, item: &TreeItem, parent: &str, cx: &mut Context<Self>) {
        if !self.has_tree_item(item) || !item.accepts_parent(parent) || !self.tree_editable(cx) {
            return;
        }
        self.library_action(
            item.operation(LibraryAction::Move {
                from: item.path.clone(),
                to: child_path(parent, document_parts(&item.path).1),
            }),
            cx,
        );
    }

    pub(super) fn reorder_tree_item(
        &mut self,
        item: &TreeItem,
        sibling: String,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.has_tree_item(item)
            || !item.accepts_parent(document_parts(&sibling).0)
            || !self.tree_editable(cx)
        {
            return;
        }
        self.library_action(
            item.operation(LibraryAction::Reorder {
                path: item.path.clone(),
                sibling,
                after,
            }),
            cx,
        );
    }

    pub(super) fn open_move_picker(
        &mut self,
        item: &TreeItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.library_mode(Panel::Move, cx);
        let mut folders = vec![String::new()];
        folders.extend(
            self.knowledge
                .tree
                .iter()
                .filter(|r| r.id.is_none() && item.accepts_parent(&r.path))
                .map(|r| r.path.clone()),
        );
        let cursor = folders
            .iter()
            .position(|p| p == document_parts(&item.path).0)
            .unwrap_or(0);
        let focus = cx.focus_handle();
        let scroll = gpui::UniformListScrollHandle::new();
        scroll.scroll_to_item(cursor, gpui::ScrollStrategy::Nearest);
        self.knowledge.move_picker = Some(MovePicker {
            item: item.clone(),
            folders,
            cursor,
            focus: focus.clone(),
            scroll,
        });
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    fn choose_move_folder(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &self.knowledge.move_picker
            && let Some(folder) = picker.folders.get(index).cloned()
        {
            let item = picker.item.clone();
            self.move_tree_item(&item, &folder, cx);
            self.focus_knowledge(window, cx);
        }
    }

    /// One focus stop handles the complete virtualized directory chooser
    pub(super) fn render_move_picker(
        &self,
        base: Div,
        picker: &MovePicker,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let list = gpui::uniform_list(
            "knowledge-move-folders",
            picker.folders.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let picker = this.knowledge.move_picker.as_ref().unwrap();
                range
                    .map(|i| {
                        let path = &picker.folders[i];
                        let label = if path.is_empty() {
                            tr!("knowledge.root_folder")
                        } else {
                            path.clone()
                        };
                        div()
                            .id(("knowledge-move-folder", i))
                            .h(px(30.))
                            .px(px(8.))
                            .flex()
                            .gap(px(6.))
                            .items_center()
                            .rounded(px(5.))
                            .when(i == picker.cursor, |row| {
                                row.bg(theme.overlay_strong)
                                    .font_weight(FontWeight::SEMIBOLD)
                            })
                            .hover(|s| s.bg(theme.overlay))
                            .child(icon("icons/folder.svg", 14., theme.text_secondary))
                            .child(div().truncate().child(label))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose_move_folder(i, window, cx)
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&picker.scroll)
        .flex_1()
        .min_h_0();
        base.p(px(14.))
            .gap(px(8.))
            .child(tr!("knowledge.target_folder"))
            .child(picker.item.path.clone())
            .child(
                div()
                    .id("knowledge-move-picker")
                    .track_focus(&picker.focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .focus_visible(|s| s.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        let Some(picker) = this.knowledge.move_picker.as_mut() else {
                            return;
                        };
                        match event.keystroke.key.as_str() {
                            "up" => picker.cursor = picker.cursor.saturating_sub(1),
                            "down" => {
                                picker.cursor = (picker.cursor + 1).min(picker.folders.len() - 1)
                            }
                            "home" => picker.cursor = 0,
                            "end" => picker.cursor = picker.folders.len() - 1,
                            "enter" | "space" => {
                                let index = picker.cursor;
                                this.choose_move_folder(index, window, cx);
                                cx.stop_propagation();
                                return;
                            }
                            "escape" => {
                                this.library_mode(Panel::Preview, cx);
                                this.focus_knowledge(window, cx);
                                cx.stop_propagation();
                                return;
                            }
                            _ => return,
                        }
                        picker
                            .scroll
                            .scroll_to_item(picker.cursor, gpui::ScrollStrategy::Nearest);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(list),
            )
            .child(assistant_button(
                "cancel-library-move",
                tr!("common.cancel"),
                cx,
                |this, window, cx| {
                    this.library_mode(Panel::Preview, cx);
                    this.focus_knowledge(window, cx);
                },
            ))
            .into_any_element()
    }

    /// Track the pointer within native row bounds and expand only after a sustained center hover
    pub(super) fn tree_drag_row(
        &self,
        row: Stateful<Div>,
        item: TreeItem,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let view = cx.weak_entity();
        let target = item.clone();
        let hover = item.clone();
        let hinted = self
            .knowledge
            .drop_hint
            .as_ref()
            .filter(|(p, _)| p == &item.path && cx.has_active_drag())
            .map(|(_, z)| *z);
        row.when(hinted == Some(DropZone::Inside), |s| {
            s.bg(theme.overlay_strong)
                .border_1()
                .border_color(theme.accent)
        })
        .when(hinted == Some(DropZone::Before), |s| {
            s.border_t_2().border_color(theme.accent)
        })
        .when(hinted == Some(DropZone::After), |s| {
            s.border_b_2().border_color(theme.accent)
        })
        .on_drag(item, move |item, _, _, cx| {
            let _ = view.update(cx, |this, _| {
                this.knowledge.drop_hint = None;
                this.knowledge.hover_expand = None;
            });
            cx.new(|_| item.clone())
        })
        .on_drag_move::<TreeItem>(cx.listener(
            move |this, event: &gpui::DragMoveEvent<TreeItem>, window, cx| {
                let source = event.drag(cx);
                let bounds = event.bounds;
                let pos = event.event.position;
                let zone = drop_zone(
                    hover.id.is_none(),
                    pos.y - bounds.origin.y,
                    bounds.size.height,
                );
                let pinned = |id: Option<Uuid>| {
                    id.is_some_and(|id| this.state.pinned_documents.contains(&id))
                };
                let allowed = bounds.contains(&pos)
                    && hover.root == source.root
                    && hover.path != source.path
                    && if zone == DropZone::Inside {
                        source.accepts_parent(&hover.path)
                    } else {
                        source.accepts_parent(document_parts(&hover.path).0)
                            && pinned(source.id) == pinned(hover.id)
                    };
                let page = &mut this.knowledge;
                if !allowed {
                    if page
                        .drop_hint
                        .as_ref()
                        .is_some_and(|(path, _)| path == &hover.path)
                    {
                        page.drop_hint = None;
                        page.hover_expand = None;
                    }
                    return;
                }
                if page
                    .drop_hint
                    .as_ref()
                    .is_some_and(|(p, z)| p == &hover.path && *z == zone)
                {
                    return;
                }
                page.drop_hint = Some((hover.path.clone(), zone));
                page.hover_expand = None;
                if zone == DropZone::Inside && page.collapsed.contains(&hover.path) {
                    let path = hover.path.clone();
                    page.hover_expand = Some(cx.spawn_in(window, async move |view, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(550))
                            .await;
                        let _ = view.update_in(cx, |this, window, cx| {
                            if cx.has_active_drag()
                                && bounds.contains(&window.mouse_position())
                                && this.knowledge.collapsed.remove(&path)
                            {
                                this.knowledge.refresh_visible();
                                cx.notify();
                            }
                        });
                    }));
                }
            },
        ))
        .on_drop(cx.listener(move |this, source: &TreeItem, _, cx| {
            let hint = this.knowledge.drop_hint.take();
            this.knowledge.hover_expand = None;
            if let Some((path, zone)) = hint
                && path == target.path
                && this.has_tree_item(&target)
                && source.root == target.root
            {
                match zone {
                    DropZone::Inside => this.move_tree_item(source, &path, cx),
                    DropZone::Before | DropZone::After => {
                        this.reorder_tree_item(source, path, zone == DropZone::After, cx)
                    }
                }
            }
            cx.stop_propagation();
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The center of a folder accepts children and the edges insert adjacent siblings
    #[test]
    fn tree_drop_targets_respect_edges_and_path_components() {
        use DropZone::*;
        assert!(drop_zone(true, px(2.), px(28.)) == Before);
        assert!(drop_zone(true, px(14.), px(28.)) == Inside);
        assert!(drop_zone(true, px(26.), px(28.)) == After);
        assert!(drop_zone(false, px(12.), px(28.)) == Before);
        assert!(drop_zone(false, px(14.), px(28.)) == After);
        let item = TreeItem {
            root: "test".into(),
            path: "会议".into(),
            id: None,
        };
        assert!(!item.accepts_parent("会议") && !item.accepts_parent("会议/记录"));
        assert!(item.accepts_parent("") && item.accepts_parent("会议记录"));
    }
}
