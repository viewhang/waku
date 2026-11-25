//! Document row menus bind their actions to catalog IDs independently of the main preview

use super::*;
use std::io::Write;

#[derive(Clone, Copy)]
enum RowAction {
    OpenWindow,
    Duplicate,
    Move,
    CopyPath,
    Pin,
    Favorite,
    Download,
    Rename,
    Delete,
}

/// Catalog paths use a slash independently of the client platform
pub(super) fn document_parts(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Replacing the selected basename keeps the extension visible and editable
fn edit_filename(input: &mut TextInput, name: &str, cx: &mut Context<TextInput>) {
    input.set_read_only(false);
    input.set_content(name.to_owned(), cx);
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    input.select_range(0..stem.len(), cx);
}

impl Waku {
    /// The row and its ellipsis share a single menu and document target
    pub(super) fn with_library_row_menu(
        &self,
        row: Stateful<Div>,
        id: Uuid,
        path: &str,
        group: SharedString,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let key: SharedString = format!("knowledge-actions-{id}").into();
        // Virtualized rows release their focus and menu state when they leave the viewport
        let menu = window
            .use_keyed_state(key.clone(), cx, |_, cx| ContextMenuHandle::new(cx))
            .read(cx)
            .clone();
        let weak = cx.weak_entity();
        let doc = LibraryDocument {
            id,
            path: path.into(),
        };
        let pinned = self.state.pinned_documents.contains(&id);
        let favorite = self.state.favorite_documents.contains(&id);
        let trigger = icon_button(key.clone(), "icons/ellipsis.svg", theme)
            .flex_none()
            .when(!selected && !menu.is_open(), |e| {
                e.opacity(0.).group_hover(group, |s| s.opacity(1.))
            })
            .focus_visible(|s| s.opacity(1.).border_1().border_color(theme.accent))
            .when(menu.is_open(), |e| e.bg(theme.overlay_strong))
            .on_click(|_, _, cx| cx.stop_propagation())
            .tooltip(Tooltip::text(tr!("knowledge.document_actions")));
        let dropdown = dropdown_menu(trigger, key, &menu, MenuAlign::BelowLeft, move |_| {
            let item = |label: &str, icon, action| {
                let weak = weak.clone();
                let doc = doc.clone();
                MenuItem::new(tr!(label), move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.document_row_action(action, &doc, window, cx)
                    });
                })
                .icon(icon)
            };
            let download = item("knowledge.original", "icons/file.svg", RowAction::Download);
            vec![
                item(
                    "knowledge.open_window",
                    "icons/window-restore.svg",
                    RowAction::OpenWindow,
                ),
                item(
                    "knowledge.duplicate",
                    "icons/copy.svg",
                    RowAction::Duplicate,
                ),
                item("knowledge.move", "icons/folder-open.svg", RowAction::Move),
                item(
                    "knowledge.copy_address",
                    "icons/copy.svg",
                    RowAction::CopyPath,
                ),
                item(
                    if pinned {
                        "knowledge.unpin"
                    } else {
                        "knowledge.pin"
                    },
                    "icons/pin.svg",
                    RowAction::Pin,
                ),
                item(
                    if favorite {
                        "knowledge.unfavorite"
                    } else {
                        "knowledge.favorite"
                    },
                    "icons/star.svg",
                    RowAction::Favorite,
                ),
                MenuItem::Separator,
                MenuItem::submenu(tr!("knowledge.download"), move |_| vec![download.clone()])
                    .icon("icons/download.svg"),
                item("common.rename", "icons/pencil.svg", RowAction::Rename),
                MenuItem::Separator,
                item("skills.delete", "icons/trash.svg", RowAction::Delete).destructive(),
            ]
        });
        row.child(dropdown)
            .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                menu.open_context_menu_at(event.position, window, cx);
                window.prevent_default();
                cx.stop_propagation();
            })
    }

    /// Resolve the clicked row again after a menu delay before acting on its cached path
    fn document_row_action(
        &mut self,
        action: RowAction,
        doc: &LibraryDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .knowledge
            .tree
            .iter()
            .any(|row| row.id == Some(doc.id) && row.path == doc.path)
        {
            return;
        }
        match action {
            RowAction::OpenWindow | RowAction::Download => self.read_row_source(doc, action, cx),
            RowAction::CopyPath => {
                if let Some(config) = &self.knowledge.status.config {
                    let path = Path::new(&config.root).join(&doc.path);
                    cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
                    self.show_info_toast(tr!("common.copied"));
                }
            }
            RowAction::Pin | RowAction::Favorite => {
                let pin = matches!(action, RowAction::Pin);
                let marks = if pin {
                    &mut self.state.pinned_documents
                } else {
                    &mut self.state.favorite_documents
                };
                if !marks.remove(&doc.id) {
                    marks.insert(doc.id);
                }
                self.save();
                if pin {
                    self.knowledge.loaded = None;
                    self.poll_library(cx);
                }
            }
            _ if self.knowledge.busy
                || self.knowledge.rename_save.is_some()
                || self.has_library_draft(cx) =>
            {
                return;
            }
            RowAction::Rename => {
                let page = &mut self.knowledge;
                page.rename_target = Some(doc.id);
                page.select_document(doc.id);
                let (_, name) = document_parts(&doc.path);
                page.rename_input
                    .update(cx, |input, cx| edit_filename(input, name, cx));
                let focus = page.rename_input.read(cx).focus();
                window.on_next_frame(move |window, cx| window.focus(&focus, cx));
            }
            RowAction::Move => {
                self.library_mode(Panel::Move, cx);
                self.knowledge.file_target = Some(doc.clone());
                self.knowledge.path_input.update(cx, |input, cx| {
                    input.set_placeholder("", cx);
                    input.set_content(document_parts(&doc.path).0.to_owned(), cx);
                    input.select_all_text(cx);
                });
                let focus = self.knowledge.path_input.read(cx).focus();
                window.on_next_frame(move |window, cx| window.focus(&focus, cx));
            }
            RowAction::Duplicate => {
                let (_, filename) = document_parts(&doc.path);
                let Some((stem, ext)) = filename.rsplit_once('.') else {
                    return;
                };
                let folder = &doc.path[..doc.path.len() - filename.len()];
                let base = tr!("knowledge.copy_name", name = stem);
                let paths: HashSet<_> = self
                    .knowledge
                    .tree
                    .iter()
                    .map(|row| row.path.as_str())
                    .collect();
                for number in 1.. {
                    let name = if number == 1 {
                        format!("{base}.{ext}")
                    } else {
                        format!("{base} {number}.{ext}")
                    };
                    let to = format!("{folder}{name}");
                    if !paths.contains(to.as_str()) {
                        self.library_action(
                            KnowledgeOperation::DocumentFile {
                                id: doc.id,
                                action: DocumentFileAction::Duplicate { to },
                            },
                            cx,
                        );
                        break;
                    }
                }
            }
            RowAction::Delete => {
                let confirm = window.prompt(
                    gpui::PromptLevel::Warning,
                    &tr!("knowledge.delete_confirm", path = &doc.path),
                    None,
                    &[
                        gpui::PromptButton::cancel(tr!("common.cancel")),
                        gpui::PromptButton::new(tr!("skills.delete")),
                    ],
                    cx,
                );
                let id = doc.id;
                let revision = self.knowledge.revision;
                cx.spawn(async move |view, cx| {
                    if confirm.await == Ok(1) {
                        let _ = view.update(cx, |this, cx| {
                            if this.knowledge.revision == revision && !this.has_library_draft(cx) {
                                this.library_action(
                                    KnowledgeOperation::DocumentFile {
                                        id,
                                        action: DocumentFileAction::Delete,
                                    },
                                    cx,
                                );
                            }
                        });
                    }
                })
                .detach();
            }
        }
        cx.notify();
    }

    /// A valid click saves the name and continues to the original target, including another row
    pub(super) fn click_outside_document_rename(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left {
            let name = self.knowledge.rename_input.read(cx).content().to_owned();
            if !self.submit_document_rename(&name, window, cx) {
                window.prevent_default();
                cx.stop_propagation();
            }
        }
    }

    /// Return false only when the current draft needs correction before an outside click can proceed
    pub(super) fn submit_document_rename(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let page = &mut self.knowledge;
        let Some(id) = page.rename_target.filter(|_| page.rename_save.is_none()) else {
            return true;
        };
        let Some(row) = page.tree.iter().find(|row| row.id == Some(id)) else {
            return true;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(['/', '\\']) {
            self.show_error_toast(tr!("knowledge.invalid_name"));
            cx.notify();
            return false;
        }
        if name == row.name {
            page.rename_target = None;
            self.focus_knowledge(window, cx);
            cx.notify();
            return true;
        }
        let to = format!("{}{name}", &row.path[..row.path.len() - row.name.len()]);
        let operation = KnowledgeOperation::DocumentFile {
            id,
            action: DocumentFileAction::Move { to: to.clone() },
        };
        page.rename_input
            .update(cx, |input, _| input.set_read_only(true));
        let client = self.daemon.client();
        page.rename_save = Some(cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { request(&client, operation) })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                let page = &mut this.knowledge;
                page.rename_save = None;
                page.rename_input
                    .update(cx, |input, _| input.set_read_only(false));
                // When the click moved focus elsewhere, report a failed save without leaving a stale editor
                let focused = page.rename_input.read(cx).focus().is_focused(window);
                if !focused {
                    page.rename_target = None;
                }
                match result {
                    Ok(KnowledgeResult::Managed { .. }) => {
                        // Paint the confirmed name immediately while the catalog refresh sorts the rows
                        if let Some(row) = page.tree.iter_mut().find(|row| row.id == Some(id)) {
                            row.name = document_parts(&to).1.to_owned();
                            row.path.clone_from(&to);
                        }
                        if let Some(loc) = &mut page.location
                            && loc.id == id
                        {
                            loc.path = to;
                        }
                        page.loaded = None;
                        page.rename_target = None;
                        if focused {
                            window.focus(&page.tree_focus, cx);
                        }
                        this.poll_library(cx);
                    }
                    Ok(_) => this.show_error_toast(tr!("assistant.invalid_response")),
                    Err(error) => this.show_error_toast(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
        true
    }

    /// Moving to another directory preserves the original filename
    pub(super) fn save_file_destination(&mut self, cx: &mut Context<Self>) {
        let p = &self.knowledge;
        let Some(doc) = &p.file_target else { return };
        let input = p.path_input.read(cx).content().trim();
        let (_, old_name) = document_parts(&doc.path);
        let to = if input.is_empty() {
            old_name.to_string()
        } else {
            format!("{}/{old_name}", input.trim_end_matches('/'))
        };
        if to == doc.path {
            self.library_mode(Panel::Preview, cx);
            return;
        }
        self.library_action(
            KnowledgeOperation::DocumentFile {
                id: doc.id,
                action: DocumentFileAction::Move { to },
            },
            cx,
        );
    }

    /// Export writes the full original source and an independent window owns its own reading position
    fn read_row_source(
        &mut self,
        doc: &LibraryDocument,
        action: RowAction,
        cx: &mut Context<Self>,
    ) {
        let save = matches!(action, RowAction::Download).then(|| {
            let directory = dirs::download_dir().unwrap_or_default();
            cx.prompt_for_new_path(&directory, Some(document_parts(&doc.path).1))
        });
        let client = self.daemon.client();
        let id = doc.id;
        cx.spawn(async move |view, cx| {
            let result: anyhow::Result<_> = async {
                let dest = match save {
                    Some(picker) => match picker.await?? {
                        Some(path) => Some(path),
                        None => return Ok(None),
                    },
                    None => None,
                };
                cx.background_executor()
                    .spawn(async move {
                        let KnowledgeResult::Source { path, text, .. } = request(
                            &client,
                            KnowledgeOperation::DocumentFile {
                                id,
                                action: DocumentFileAction::Source,
                            },
                        )?
                        else {
                            anyhow::bail!(tr!("assistant.invalid_response"));
                        };
                        if let Some(dest) = dest {
                            let mut file =
                                tempfile::NamedTempFile::new_in(dest.parent().ok_or_else(
                                    || anyhow::anyhow!("Missing destination directory"),
                                )?)?;
                            file.write_all(text.as_bytes())?;
                            file.persist(&dest)?;
                            Ok(None)
                        } else {
                            Ok(Some((path, PreparedDocument::new(&text, 0))))
                        }
                    })
                    .await
            }
            .await;
            let _ = view.update(cx, |this, cx| {
                let result = result.and_then(|source| {
                    if let Some((path, document)) = source {
                        let metrics = MarkdownMetrics::document(
                            this.state.ui_font_size,
                            this.state.code_font_size,
                        );
                        let math = this.state.render_math;
                        cx.open_window(gpui::WindowOptions::default(), move |window, cx| {
                            window.set_window_title(&path);
                            cx.new(|cx| {
                                let preview = DocumentPreview::new(document, cx);
                                window.focus(&preview.focus, cx);
                                reader::DocumentWindow {
                                    preview,
                                    metrics,
                                    math,
                                }
                            })
                        })?;
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    this.show_error_toast(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typing over a Chinese filename replaces the selected stem and leaves the suffix editable
    #[gpui::test]
    fn inline_rename_preserves_the_extension(cx: &mut gpui::TestAppContext) {
        let (field, cx) = cx.add_window_view(|window, cx| {
            let mut field = TextInput::new(window, cx).submit_on_enter();
            edit_filename(&mut field, "会议.初稿.md", cx);
            field
        });
        cx.update(|window, cx| window.focus(&field.read(cx).focus(), cx));
        cx.simulate_input("定稿");
        cx.read_entity(&field, |field, _| assert_eq!(field.content(), "定稿.md"));
    }
}
