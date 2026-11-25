//! Library membership and the active library's indexing preferences share the native settings page

use super::*;

impl KnowledgePage {
    pub(super) fn adding(&self) -> bool {
        self.adding_library || self.status.config.is_none()
    }

    /// Each form keeps its own model until the user saves it
    pub(in crate::app) fn settings_model(&self) -> Option<&KnowledgeModel> {
        if self.adding() {
            self.new_embedding.as_ref()
        } else {
            self.embedding.as_ref()
        }
    }

    pub(in crate::app) fn select_settings_model(&mut self, model: KnowledgeModel) {
        if self.changing_libraries {
            return;
        }
        if self.adding() {
            self.adding_library = true;
            self.new_embedding = Some(model);
        } else {
            self.embedding = Some(model);
        }
    }

    pub(super) fn close_add_form(&mut self, cx: &mut Context<Waku>) {
        self.adding_library = false;
        self.new_embedding = None;
        self.selected_roots.clear();
        self.root.update(cx, |input, cx| input.clear(cx));
    }

    /// A model change always targets the saved active root, independently of the folder input
    fn can_save_settings(&self, cx: &App) -> bool {
        if self.busy || self.settings_model().is_none() {
            return false;
        }
        if self.adding() {
            !self.selected_roots.is_empty() || !self.root.read(cx).content().trim().is_empty()
        } else {
            self.status
                .config
                .as_ref()
                .is_some_and(|config| Some(&config.embedding) != self.embedding.as_ref())
        }
    }
}

/// Folder drafts belong to additions, while model edits use the saved library identity
fn settings_operation(
    current: Option<&LibraryConfig>,
    embedding: KnowledgeModel,
    picked: &[String],
    typed: &str,
) -> KnowledgeOperation {
    if let Some(current) = current {
        return KnowledgeOperation::Configure {
            config: LibraryConfig {
                root: current.root.clone(),
                embedding,
            },
        };
    }
    let roots = if picked.is_empty() {
        vec![typed.trim().to_owned()]
    } else {
        picked.to_vec()
    };
    KnowledgeOperation::AddLibraries { roots, embedding }
}

impl Waku {
    fn save_library_settings(&mut self, cx: &mut Context<Self>) {
        let p = &self.knowledge;
        if !p.can_save_settings(cx) {
            return;
        }
        let Some(embedding) = p.settings_model().cloned() else {
            return;
        };
        let current = p.status.config.as_ref().filter(|_| !p.adding());
        let operation = settings_operation(
            current,
            embedding,
            &p.selected_roots,
            p.root.read(cx).content(),
        );
        self.library_action(operation, cx);
    }

    /// Show full paths for disambiguation while keeping the folder name easy to scan
    fn saved_library_row(&self, index: usize, item: &LibraryConfig, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let root = item.root.clone();
        let active = self
            .knowledge
            .status
            .config
            .as_ref()
            .is_some_and(|c| c.root == root);
        let remove_root = root.clone();
        let name = library_folder_name(&root).to_owned();
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(10.))
            .py(px(9.))
            .when(index > 0, |row| row.border_t_1().border_color(theme.border))
            .child(icon("icons/folder.svg", 17., theme.text_secondary))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(name),
                            )
                            .when(active, |line| {
                                line.child(
                                    div()
                                        .flex_none()
                                        .text_size(sp(11.5))
                                        .text_color(theme.text_secondary)
                                        .child(tr!("knowledge.active_library")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id(("library-setting-path", index))
                            .truncate()
                            .text_color(theme.text_secondary)
                            .text_size(sp(12.))
                            .child(root.clone())
                            .tooltip(Tooltip::text(root.clone())),
                    ),
            )
            .child(
                assistant_button(
                    ("open-library-folder", index),
                    tr!("knowledge.open_in_file_manager"),
                    cx,
                    move |this, _, cx| {
                        if this.daemon.is_remote() {
                            this.show_info_toast(tr!("errors.remote_host_path"));
                            cx.notify();
                        } else {
                            crate::platform::open_with_default_app(Path::new(&root), cx);
                        }
                    },
                )
                .self_center(),
            )
            .child(
                icon_button(("remove-library", index), "icons/x.svg", theme)
                    .size(px(28.))
                    .flex_none()
                    .tab_index(0)
                    .focus_visible(|s| s.border_1().border_color(theme.accent))
                    .tooltip(Tooltip::text(tr!(
                        "knowledge.remove_library",
                        name = library_folder_name(&remove_root)
                    )))
                    .on_activation(cx, move |this, _, cx| {
                        this.library_action(
                            KnowledgeOperation::RemoveLibrary {
                                root: remove_root.clone(),
                            },
                            cx,
                        )
                    }),
            )
    }

    pub(in crate::app) fn render_library_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.knowledge;
        let theme = Theme::current(cx);
        let card = || {
            div()
                .w_full()
                .px(px(18.))
                .py(px(13.))
                .rounded(px(13.))
                .bg(theme.raised)
                .flex()
                .flex_col()
                .gap(px(9.))
        };
        let heading = |title: String| {
            div()
                .min_w_0()
                .flex_1()
                .font_weight(FontWeight::MEDIUM)
                .child(title)
        };
        let adding = p.adding();
        let mut page = div()
            .mt(px(15.))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(14.))
            .text_size(sp(13.))
            .text_color(theme.text);
        if !p.status.libraries.is_empty() {
            let libraries = card()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(heading(tr!("knowledge.saved_libraries")))
                        .when(!adding, |bar| {
                            bar.child(assistant_button(
                                "add-library",
                                tr!("knowledge.add_library"),
                                cx,
                                |this, window, cx| {
                                    if this.knowledge.changing_libraries {
                                        return;
                                    }
                                    let p = &mut this.knowledge;
                                    p.adding_library = true;
                                    p.new_embedding =
                                        p.status.config.as_ref().map(|c| c.embedding.clone());
                                    let focus = p.root.read(cx).focus();
                                    window
                                        .on_next_frame(move |window, cx| window.focus(&focus, cx));
                                    cx.notify();
                                },
                            ))
                        }),
                )
                .child(
                    div().flex().flex_col().children(
                        p.status
                            .libraries
                            .iter()
                            .enumerate()
                            .map(|(i, item)| self.saved_library_row(i, item, cx)),
                    ),
                );
            page = page.child(libraries);
        }

        let mut form = card().child(
            heading(if adding {
                tr!("knowledge.add_library")
            } else {
                tr!("knowledge.index_settings")
            })
            .flex_none(),
        );
        if adding {
            let mut directories = div().flex().items_center().gap(px(8.));
            if p.selected_roots.is_empty() {
                directories = directories.child(
                    TextField::new("library-root", p.root.clone())
                        .icon("icons/folder.svg", 14.)
                        .flex_1()
                        .min_w_0(),
                );
            }
            if !self.daemon.is_remote() {
                directories = directories.child(
                    assistant_button(
                        "library-choose",
                        tr!("knowledge.choose"),
                        cx,
                        |this, _, cx| this.choose_library_folder(cx),
                    )
                    .self_center(),
                );
            }
            form =
                form.child(div().font_weight(FontWeight::MEDIUM).child(
                    if self.daemon.is_remote() {
                        tr!("knowledge.remote_root")
                    } else {
                        tr!("knowledge.root")
                    },
                ))
                .children(p.selected_roots.iter().enumerate().map(|(i, root)| {
                    let path = root.clone();
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(icon("icons/folder.svg", 14., theme.text_tertiary))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .text_size(sp(12.))
                                .child(root.clone()),
                        )
                        .child(
                            icon_button(("unstage-library", i), "icons/x.svg", theme)
                                .tab_index(0)
                                .focus_visible(|s| s.border_1().border_color(theme.accent))
                                .tooltip(Tooltip::text(tr!(
                                    "knowledge.remove_library",
                                    name = library_folder_name(root)
                                )))
                                .on_activation(cx, move |this, _, cx| {
                                    if this.knowledge.changing_libraries {
                                        return;
                                    }
                                    this.knowledge.selected_roots.retain(|item| item != &path);
                                    cx.notify();
                                }),
                        )
                }))
                .child(directories);
        }
        form = form
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(tr!("knowledge.embedding"))
                    .child(self.knowledge_embedding_picker(cx)),
            )
            .child(
                div()
                    .text_size(sp(12.))
                    .line_height(sp(17.))
                    .text_color(theme.text_secondary)
                    .child(tr!("knowledge.disclosure")),
            );

        let dirty = p
            .status
            .config
            .as_ref()
            .is_some_and(|c| Some(&c.embedding) != p.embedding.as_ref());
        if adding || dirty {
            let label = if adding {
                tr!("knowledge.apply")
            } else {
                tr!("knowledge.save")
            };
            let save = assistant_button_base("library-apply", label, theme).when_else(
                p.can_save_settings(cx),
                |button| {
                    button
                        .hover(|s| s.bg(theme.overlay))
                        .on_activation(cx, |this, _, cx| this.save_library_settings(cx))
                },
                |button| button.opacity(0.5).tab_stop(false),
            );
            form = form.child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.))
                    .when(p.status.config.is_some(), |row| {
                        row.child(assistant_button(
                            "cancel-library-settings",
                            tr!("common.cancel"),
                            cx,
                            |this, _, cx| {
                                if this.knowledge.changing_libraries {
                                    return;
                                }
                                let p = &mut this.knowledge;
                                if p.adding() {
                                    p.close_add_form(cx);
                                } else {
                                    p.embedding =
                                        p.status.config.as_ref().map(|c| c.embedding.clone());
                                }
                                cx.notify();
                            },
                        ))
                    })
                    .child(save),
            );
        }
        if !adding {
            form = form.child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt(px(10.))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(tr!("knowledge.document_index"))
                    .when_else(
                        p.status.indexing,
                        |row| {
                            row.child(div().text_color(theme.text_secondary).child(tr!(
                                "knowledge.indexing",
                                ready = p.status.ready,
                                total = p.status.total
                            )))
                        },
                        |row| row.child(self.library_reindex_button(cx).self_center()),
                    ),
            );
        }
        page.child(form)
            .when(p.changing_libraries, |page| {
                page.child(
                    div()
                        .text_color(theme.text_secondary)
                        .child(tr!("knowledge.working")),
                )
            })
            .children(
                p.error
                    .as_ref()
                    .or(p.poll_error.as_ref())
                    .or(p.status.error.as_ref())
                    .map(|error| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_color(theme.danger)
                            .child(icon("icons/alert.svg", 14., theme.danger))
                            .child(error.clone())
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unsubmitted folder draft must never redirect an active library's model update
    #[test]
    fn save_targets_the_current_library_or_the_selected_folders() {
        let model = KnowledgeModel {
            service_id: Uuid::nil(),
            model: "encoder".into(),
        };
        let current = LibraryConfig {
            root: "/notes/work".into(),
            embedding: model.clone(),
        };
        let updated = KnowledgeModel {
            model: "encoder-v2".into(),
            ..model
        };
        let folders = vec!["/notes/personal".into(), "/notes/project".into()];
        let typed = "  /remote/notes  ";
        let KnowledgeOperation::Configure { config } =
            settings_operation(Some(&current), updated.clone(), &folders, typed)
        else {
            panic!("expected a model update")
        };
        assert_eq!(config.root, current.root);
        assert_eq!(config.embedding, updated);
        let KnowledgeOperation::AddLibraries { roots, .. } =
            settings_operation(None, updated.clone(), &folders, typed)
        else {
            panic!("expected the folder selection")
        };
        assert_eq!(roots, folders);
        let KnowledgeOperation::AddLibraries { roots, .. } =
            settings_operation(None, updated, &[], typed)
        else {
            panic!("expected the typed path")
        };
        assert_eq!(roots, ["/remote/notes"]);
    }
}
