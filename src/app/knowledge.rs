//! Personal library configuration, document browsing and indexing use the daemon transport

use super::assistants::{assistant_button, assistant_button_base};
use super::*;
use crate::ui::ActivationExt;
use waku_protocol::knowledge::*;
use waku_protocol::{Command, ResponsePayload};
mod reader;
mod row_actions;
mod tree_actions;
use tree_actions::{DropZone, MovePicker, TreeItem};
mod settings;
use reader::{DocumentPreview, PreparedDocument};

pub(super) struct KnowledgePage {
    window: gpui::AnyWindowHandle,
    root: Entity<TextInput>,
    /// Folder picker selection staged until its embedding model is confirmed
    selected_roots: Vec<String>,
    changing_libraries: bool,
    /// Model edits for the current library survive opening and cancelling the add form
    embedding: Option<KnowledgeModel>,
    adding_library: bool,
    new_embedding: Option<KnowledgeModel>,
    status: LibraryStatus,
    poll: Option<gpui::Task<()>>,
    tree: Vec<TreeRow>,
    visible: Vec<usize>,
    collapsed: HashSet<String>,
    tree_scroll: gpui::UniformListScrollHandle,
    tree_focus: FocusHandle,
    cursor: usize,
    loaded: Option<(String, String)>,
    pub(super) open: bool,
    pending_source: Option<KnowledgeOperation>,
    after_create: Option<String>,
    /// Restore the manipulated row independently of the open document
    reveal_after_move: Option<String>,
    panel: Panel,
    path_input: Entity<TextInput>,
    /// Directory operations remember the library and the row that opened the menu
    move_picker: Option<MovePicker>,
    rename_target: Option<TreeItem>,
    drop_hint: Option<(String, DropZone)>,
    hover_expand: Option<gpui::Task<()>>,
    /// Keep the pending rename valid when the same mouse click opens another preview
    rename_save: Option<gpui::Task<()>>,
    rename_input: Entity<TextInput>,
    url_input: Entity<TextInput>,
    title_input: Entity<TextInput>,
    editor: Entity<TextInput>,
    versions: Vec<LibraryVersion>,
    edit_revision: Option<String>,
    edit_original: SharedString,
    edit_conflicted: bool,

    revision: u64,
    busy: bool,
    error: Option<String>,
    poll_error: Option<String>,
    location: Option<PreviewLocation>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Panel {
    Preview,
    EditNew,
    Import,
    Move,
    Edit,
    History,
}

/// Parents precede their children in the cached flattened tree
struct TreeRow {
    path: String,
    name: String,
    id: Option<Uuid>,
    revision: Option<String>,
    depth: usize,
    /// Precomputed while sorting so an open row menu never traverses the full directory
    can_move_up: bool,
    can_move_down: bool,
}

struct PreviewLocation {
    revision: String,
    id: Uuid,
    path: String,
    changed: bool,
    document: DocumentPreview,
}

/// Only desktop previews replace a passage request with a parsed full document
enum LibraryResponse {
    Command(KnowledgeResult),
    Preview(LoadedPreview),
}

struct LoadedPreview {
    id: Uuid,
    path: String,
    revision: String,
    changed: bool,
    document: PreparedDocument,
}

impl LoadedPreview {
    /// Install parsed Markdown on the UI thread after the source was read in the background
    fn into_location(self, cx: &mut App) -> PreviewLocation {
        PreviewLocation {
            id: self.id,
            path: self.path,
            revision: self.revision,
            changed: self.changed,
            document: DocumentPreview::new(self.document, cx),
        }
    }
}

impl KnowledgePage {
    fn has_draft(&self, cx: &App) -> bool {
        match self.panel {
            Panel::Edit => self.editor.read(cx).content() != self.edit_original.as_ref(),
            Panel::EditNew => {
                !self.title_input.read(cx).content().is_empty()
                    || !self.editor.read(cx).content().is_empty()
            }
            Panel::Import => {
                !self.url_input.read(cx).content().is_empty()
                    || !self.path_input.read(cx).content().is_empty()
            }
            _ => false,
        }
    }

    pub(super) fn new(window: &mut Window, cx: &mut Context<Waku>) -> Self {
        let rename_input = cx.new(|cx| TextInput::new(window, cx).submit_on_enter().tab_stop());
        cx.subscribe_in(&rename_input, window, |this, _, event, window, cx| {
            if let InputEvent::Submit(name) = event {
                this.submit_document_rename(name, window, cx);
            }
        })
        .detach();
        cx.on_blur(&rename_input.read(cx).focus(), window, |this, _, cx| {
            if this.knowledge.rename_save.is_none() && this.knowledge.rename_target.take().is_some()
            {
                cx.notify();
            }
        })
        .detach();
        let root = cx.new(|cx| {
            TextInput::new(window, cx)
                .tab_stop()
                .placeholder(tr!("knowledge.path"))
        });
        cx.subscribe(&root, |this, _, event, cx| {
            if matches!(event, InputEvent::Edited)
                && this.settings_page == Some(SettingsPage::Knowledge)
            {
                cx.notify();
            }
        })
        .detach();
        let path_input = cx.new(|cx| {
            TextInput::new(window, cx)
                .tab_stop()
                .submit_on_enter()
                .placeholder(tr!("knowledge.note_path"))
        });
        let editor = cx.new(|cx| TextInput::new(window, cx).multi_line().tab_stop());
        let title_input = cx.new(|cx| TextInput::new(window, cx).tab_stop().submit_on_enter());
        cx.subscribe_in(&title_input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::Submit(_)) && this.knowledge.panel == Panel::EditNew {
                window.focus(&this.knowledge.editor.read(cx).focus(), cx);
            }
        })
        .detach();
        Self {
            window: window.window_handle(),
            root,
            adding_library: false,
            new_embedding: None,
            selected_roots: Vec::new(),
            changing_libraries: false,
            embedding: None,
            status: Default::default(),
            poll: None,
            tree: Vec::new(),
            visible: Vec::new(),
            collapsed: HashSet::new(),
            tree_scroll: gpui::UniformListScrollHandle::new(),
            tree_focus: cx.focus_handle(),
            cursor: 0,
            loaded: None,
            open: false,
            pending_source: None,
            after_create: None,
            reveal_after_move: None,
            panel: Panel::Preview,
            move_picker: None,
            drop_hint: None,
            hover_expand: None,
            rename_target: None,
            rename_save: None,
            rename_input,
            path_input,
            url_input: cx.new(|cx| {
                TextInput::new(window, cx)
                    .tab_stop()
                    .placeholder("https://…")
            }),
            title_input,
            editor,
            versions: vec![],
            edit_revision: None,
            edit_original: "".into(),
            edit_conflicted: false,
            revision: 0,
            busy: false,
            error: None,
            poll_error: None,
            location: None,
        }
    }
}

/// Write the optional title as a Markdown heading and keep the body exactly as entered
fn new_note_markdown(title: &str, body: &str) -> String {
    match (title.trim(), body) {
        ("", _) => body.into(),
        (title, "") => format!("# {title}"),
        (title, _) => format!("# {title}\n\n{body}"),
    }
}

/// The daemon host may use a different path separator from the desktop
fn library_folder_name(root: &str) -> &str {
    root.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(root)
}

/// Keep IPC and directory enumeration off the GPUI thread
fn request(
    client: &waku_client::DaemonClient,
    operation: KnowledgeOperation,
) -> anyhow::Result<KnowledgeResult> {
    match client.request(Uuid::nil(), Uuid::nil(), Command::Knowledge { operation })? {
        ResponsePayload::Knowledge { result } => Ok(result),
        _ => anyhow::bail!(tr!("assistant.invalid_response")),
    }
}

/// Read the selected local file before sending its contents to the current library
fn local_document_action(source: &Path) -> anyhow::Result<LibraryAction> {
    use anyhow::Context as _;
    use std::io::Read;

    let invalid_document = || tr!("knowledge.import_requirements");
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(invalid_document)?;
    anyhow::ensure!(is_document(source), invalid_document());
    let metadata = std::fs::metadata(source)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= MAX_NOTE_BYTES,
        invalid_document()
    );
    let mut content = String::new();
    std::fs::File::open(source)?
        .take(MAX_NOTE_BYTES + 1)
        .read_to_string(&mut content)?;
    anyhow::ensure!(content.len() as u64 <= MAX_NOTE_BYTES, invalid_document());
    Ok(LibraryAction::Create {
        path: name.to_owned(),
        content,
        format: None,
        metadata: None,
    })
}

impl Waku {
    pub(super) fn focus_knowledge(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.knowledge.tree_focus, cx);
    }

    /// Restore the active pane and its refresh loop when returning from settings
    pub(super) fn focus_active_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.knowledge.open {
            self.poll_library(cx);
            self.focus_knowledge(window, cx);
        } else {
            let focus = self
                .translation_source_focus(cx)
                .unwrap_or_else(|| self.composer_focus(cx));
            window.focus(&focus, cx);
        }
    }

    pub(super) fn open_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_page = None;
        self.leave_translation();
        self.knowledge.open = true;
        self.poll_library(cx);
        self.focus_knowledge(window, cx);
        cx.notify();
    }

    pub(super) fn close_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.knowledge.open = false;
        window.focus(&self.composer_focus(cx), cx);
        cx.notify();
    }

    /// Poll only while visible and refresh the tree when the catalog revision changes
    pub(super) fn poll_library(&mut self, cx: &mut Context<Self>) {
        if self.knowledge.changing_libraries {
            return;
        }
        self.knowledge.poll = Some(cx.spawn(async move |view, cx| {
            loop {
                let Ok(Some((client, browse, loaded, pins, preview, intent))) =
                    view.update(cx, |this, _| {
                        (this.library_visible()).then(|| {
                            (
                                this.daemon.client(),
                                this.settings_page.is_none(),
                                this.knowledge.loaded.clone(),
                                this.state.pinned_documents.clone(),
                                this.knowledge
                                    .location
                                    .as_ref()
                                    .filter(|_| {
                                        this.knowledge.panel == Panel::Preview
                                            && !this.knowledge.busy
                                    })
                                    .map(|loc| (loc.id, loc.revision.clone())),
                                this.knowledge.revision,
                            )
                        })
                    })
                else {
                    return;
                };
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let KnowledgeResult::Status { status } =
                            request(&client, KnowledgeOperation::Status)?
                        else {
                            anyhow::bail!(tr!("assistant.invalid_response"))
                        };
                        let stamp = status
                            .config
                            .as_ref()
                            .map(|c| (c.root.clone(), status.catalog_revision.clone()));
                        let rows = if browse
                            && let Some(stamp) =
                                stamp.filter(|stamp| loaded.as_ref() != Some(stamp))
                        {
                            let mut docs = Vec::new();
                            let mut folders = Vec::new();
                            let mut positions = HashMap::new();
                            loop {
                                let KnowledgeResult::Documents {
                                    documents,
                                    folders: page_folders,
                                    positions: page_positions,
                                    more,
                                } = request(
                                    &client,
                                    KnowledgeOperation::List {
                                        offset: docs.len() + folders.len(),
                                        include_folders: Some(true),
                                    },
                                )?
                                else {
                                    anyhow::bail!(tr!("assistant.invalid_response"))
                                };
                                docs.extend(documents);
                                folders.extend(page_folders);
                                positions.extend(page_positions);
                                if !more {
                                    break;
                                }
                            }
                            // Another client can switch the active library while its directory is being listed
                            let KnowledgeResult::Status { status: latest } =
                                request(&client, KnowledgeOperation::Status)?
                            else {
                                anyhow::bail!(tr!("assistant.invalid_response"))
                            };
                            if latest.config != status.config || latest.catalog_revision != stamp.1
                            {
                                return Ok((latest, None));
                            }
                            // Reparse only the open note when its indexed content has actually changed
                            let refresh = if let Some((id, revision)) = preview
                                && docs
                                    .iter()
                                    .find(|d| d.id == id)
                                    .and_then(|d| d.revision.as_ref())
                                    .is_some_and(|current| current != &revision)
                            {
                                Some(reader::load_preview(&client, id, None, 0, None)?)
                            } else {
                                None
                            };
                            Some((
                                document_tree(docs, folders, &positions, &pins),
                                stamp,
                                refresh,
                            ))
                        } else {
                            None
                        };
                        Ok::<_, anyhow::Error>((status, rows))
                    })
                    .await;
                let Ok(keep) = view.update(cx, |this, cx| {
                    if !this.library_visible() {
                        return false;
                    }
                    match result {
                        Ok((status, rows)) => {
                            // Another client can switch libraries while this editor still holds an unsaved note
                            let page = &mut this.knowledge;
                            if matches!(page.panel, Panel::Edit | Panel::EditNew)
                                && page.status.config.as_ref().map(|c| &c.root)
                                    != status.config.as_ref().map(|c| &c.root)
                                && page.has_draft(cx)
                            {
                                let message = tr!("knowledge.draft_library_changed");
                                if page.poll_error.as_ref() != Some(&message) {
                                    page.poll_error = Some(message);
                                    cx.notify();
                                }
                                return true;
                            }
                            if this.knowledge.poll_error.take().is_some() {
                                cx.notify();
                            }
                            // A completed user action may already have invalidated this directory snapshot
                            let rows = rows.filter(|_| this.knowledge.revision == intent);
                            this.apply_library_status(status, false, cx);
                            if let Some((tree, stamp, refresh)) = rows {
                                let page = &mut this.knowledge;
                                let first_load = page.loaded.is_none();
                                // Renaming and pinning can reorder siblings, so follow documents by ID
                                let cursor_row = page
                                    .visible
                                    .get(page.cursor)
                                    .map(|i| (page.tree[*i].id, page.tree[*i].path.clone()));
                                let root = stamp.0.clone();
                                page.loaded = Some(stamp);
                                page.tree = tree;
                                page.refresh_visible();
                                if let Some((id, path)) = cursor_row
                                    && let Some(pos) = page.visible.iter().position(|i| {
                                        let row = &page.tree[*i];
                                        row.id == id && (id.is_some() || row.path == path)
                                    })
                                {
                                    page.cursor = pos;
                                }
                                if let Some(update) = refresh
                                    && page.revision == intent
                                    && page.panel == Panel::Preview
                                    && page
                                        .location
                                        .as_ref()
                                        .is_some_and(|loc| loc.id == update.id)
                                {
                                    page.location = Some(update.into_location(cx));
                                }
                                if let Some(loc) = page.location.as_mut() {
                                    if let Some(row) =
                                        page.tree.iter().find(|row| row.id == Some(loc.id))
                                    {
                                        loc.path.clone_from(&row.path);
                                        if matches!(page.panel, Panel::Edit | Panel::EditNew) {
                                            page.edit_conflicted =
                                                row.revision.as_ref().is_some_and(|r| {
                                                    Some(r) != page.edit_revision.as_ref()
                                                });
                                        }
                                    } else if matches!(page.panel, Panel::Edit | Panel::EditNew) {
                                        page.edit_conflicted = true;
                                    } else {
                                        page.location = None;
                                    }
                                }
                                // A cross-library preview may finish before its directory arrives
                                if first_load
                                    && let Some(id) = page.location.as_ref().map(|loc| loc.id)
                                {
                                    page.select_document(id);
                                }
                                if let Some(path) = page.reveal_after_move.take()
                                    && let Some(i) = page.tree.iter().position(|r| r.path == path)
                                {
                                    page.reveal_tree_row(i);
                                }
                                if let Some(path) = page.after_create.take()
                                    && let Some(i) =
                                        page.tree.iter().position(|row| row.path == path)
                                {
                                    if let Some(id) = page.tree[i].id {
                                        this.read_library_document(id, cx);
                                    } else {
                                        let item = TreeItem::new(&page.tree[i], &root);
                                        page.reveal_tree_row(i);
                                        let focus = page.prepare_rename(item, cx);
                                        this.focus_library_input(focus, cx);
                                    }
                                }
                                cx.notify();
                            }
                            if this.settings_page.is_none()
                                && this.knowledge.open
                                && !this.knowledge.busy
                                && this.knowledge.rename_save.is_none()
                            {
                                if let Some(KnowledgeOperation::Read { id, .. }) =
                                    &this.knowledge.pending_source
                                {
                                    let id = *id;
                                    // Only opening a citation changes the displayed library
                                    if this.knowledge.tree.iter().any(|row| row.id == Some(id)) {
                                        if let Some(read) = this.knowledge.pending_source.take() {
                                            this.library_action(read, cx);
                                        }
                                    } else {
                                        this.library_action(
                                            KnowledgeOperation::ActivateDocumentLibrary { id },
                                            cx,
                                        );
                                    }
                                } else if this.knowledge.location.is_none()
                                    && this.knowledge.after_create.is_none()
                                    && this.knowledge.panel == Panel::Preview
                                    && this.knowledge.rename_target.is_none()
                                    && this.knowledge.error.is_none()
                                    && let Some(id) = this.knowledge.tree.iter().find_map(|r| r.id)
                                {
                                    this.read_library_document(id, cx);
                                }
                            }
                        }
                        Err(e) => {
                            let message = e.to_string();
                            if this.knowledge.poll_error.as_ref() != Some(&message) {
                                this.knowledge.poll_error = Some(message);
                                cx.notify();
                            }
                        }
                    }
                    true
                }) else {
                    return;
                };
                if !keep {
                    return;
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        }));
    }

    fn apply_library_status(
        &mut self,
        status: LibraryStatus,
        saved_form: bool,
        cx: &mut Context<Self>,
    ) {
        if self.knowledge.status == status && !saved_form {
            return;
        }
        let page = &mut self.knowledge;
        let active_changed =
            page.status.config.as_ref().map(|c| &c.root) != status.config.as_ref().map(|c| &c.root);
        if active_changed {
            page.revision += 1;
            page.busy = false;
            page.loaded = None;
            page.panel = Panel::Preview;
            page.move_picker = None;
            page.rename_target = None;
            page.rename_save = None;
            page.after_create = None;
            page.reveal_after_move = None;
            page.location = None;
            page.tree.clear();
            page.visible.clear();
            page.collapsed.clear();
            page.drop_hint = None;
            page.hover_expand = None;
            page.cursor = 0;
            page.error = None;
        }
        if page.status.config != status.config || saved_form {
            page.embedding = status.config.as_ref().map(|c| c.embedding.clone());
            if saved_form {
                page.close_add_form(cx);
            }
        }
        page.status = status;
        cx.notify();
    }

    fn choose_library_folder(&mut self, cx: &mut Context<Self>) {
        if self.knowledge.changing_libraries {
            return;
        }
        let revision = self.knowledge.revision;
        let pick = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some(tr!("knowledge.choose").into()),
        });
        cx.spawn(async move |view, cx| {
            if let Ok(Ok(Some(paths))) = pick.await
                && !paths.is_empty()
            {
                let _ = view.update(cx, |this, cx| {
                    if this.knowledge.revision != revision
                        || !this.knowledge.adding()
                        || this.settings_page != Some(SettingsPage::Knowledge)
                    {
                        return;
                    }
                    this.knowledge.selected_roots = paths
                        .into_iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect();
                    if this.knowledge.selected_roots.len() == 1 {
                        let root = this.knowledge.selected_roots.pop().unwrap();
                        this.knowledge
                            .root
                            .update(cx, |input, cx| input.set_content(root, cx));
                    } else {
                        this.knowledge.root.update(cx, |input, cx| input.clear(cx));
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// A cancelled picker or a superseded library action must never create a document
    fn import_local_document(&mut self, cx: &mut Context<Self>) {
        if self.knowledge.busy || self.has_library_draft(cx) {
            return;
        }
        self.knowledge.revision += 1;
        let revision = self.knowledge.revision;
        self.knowledge.busy = true;
        let pick = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("knowledge.import_document").into()),
        });
        cx.spawn(async move |view, cx| {
            let result: anyhow::Result<_> = async {
                let Some(source) = pick.await??.and_then(|paths| paths.into_iter().next()) else {
                    return Ok(None);
                };
                cx.background_executor()
                    .spawn(async move { local_document_action(&source).map(Some) })
                    .await
            }
            .await;
            let _ = view.update(cx, |this, cx| {
                if this.knowledge.revision != revision {
                    return;
                }
                this.knowledge.busy = false;
                if this.knowledge.open && this.settings_page.is_none() {
                    match result {
                        Ok(Some(action)) if !this.has_library_draft(cx) => {
                            this.library_action(KnowledgeOperation::Manage { action }, cx)
                        }
                        Err(error) => this.knowledge.error = Some(error.to_string()),
                        _ => {}
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A change of document or search must preserve text still waiting to be saved
    fn has_library_draft(&mut self, cx: &mut Context<Self>) -> bool {
        let p = &self.knowledge;
        let dirty = p.has_draft(cx);
        if dirty {
            self.show_info_toast(tr!("knowledge.unsaved"));
            cx.notify();
        }
        dirty
    }

    /// Each user action owns its response until another action or configuration supersedes it
    fn library_action(&mut self, operation: KnowledgeOperation, cx: &mut Context<Self>) {
        if self.knowledge.changing_libraries {
            return;
        }
        let rebuild = matches!(operation, KnowledgeOperation::Rebuild);
        let citation_id = match &operation {
            KnowledgeOperation::ActivateDocumentLibrary { id } => Some(*id),
            _ => None,
        };
        let saved_form = matches!(
            operation,
            KnowledgeOperation::Configure { .. } | KnowledgeOperation::AddLibraries { .. }
        );
        let catalog = saved_form
            || citation_id.is_some()
            || matches!(
                operation,
                KnowledgeOperation::ActivateLibrary { .. }
                    | KnowledgeOperation::RemoveLibrary { .. }
            );
        if catalog {
            if self.knowledge.busy
                || self.knowledge.rename_save.is_some()
                || self.has_library_draft(cx)
            {
                return;
            }
            self.knowledge.changing_libraries = true;
            self.knowledge.poll = None;
        }
        let created_folder = matches!(
            operation,
            KnowledgeOperation::CreateUntitled { folder: true, .. }
        );
        let managed_path = match &operation {
            KnowledgeOperation::DocumentFile {
                action: DocumentFileAction::Duplicate { to } | DocumentFileAction::Move { to },
                ..
            } => Some(to.clone()),
            KnowledgeOperation::ManageTree {
                action: LibraryAction::Update { path, .. },
                ..
            } => Some(path.clone()),
            KnowledgeOperation::Manage {
                action:
                    LibraryAction::Create { path, .. }
                    | LibraryAction::Update { path, .. }
                    | LibraryAction::Restore { path, .. }
                    | LibraryAction::ImportUrl { path, .. },
            } => Some(path.clone()),
            _ => None,
        };
        let tree_move = match &operation {
            KnowledgeOperation::ManageTree {
                action: LibraryAction::Move { from, to },
                ..
            } => Some((from.clone(), to.clone())),
            KnowledgeOperation::ManageTree {
                action: LibraryAction::Reorder { path, sibling, .. },
                ..
            } => Some((
                path.clone(),
                tree_actions::child_path(
                    row_actions::document_parts(sibling).0,
                    row_actions::document_parts(path).1,
                ),
            )),
            _ => None,
        };
        let deleted = match &operation {
            KnowledgeOperation::DocumentFile {
                id,
                action: DocumentFileAction::Delete,
            } => Some(*id),
            _ => None,
        };
        self.knowledge.revision += 1;
        self.knowledge.after_create = None;
        let revision = self.knowledge.revision;
        self.knowledge.busy = true;
        self.knowledge.error = None;
        let client = self.daemon.client();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let KnowledgeOperation::Read {
                        id,
                        revision,
                        start,
                        end,
                    } = operation
                    {
                        return reader::load_preview(&client, id, revision.as_deref(), start, end)
                            .map(LibraryResponse::Preview);
                    }
                    request(&client, operation).map(LibraryResponse::Command)
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                let page = &mut this.knowledge;
                if page.revision != revision {
                    return;
                }
                page.busy = false;
                page.changing_libraries = false;
                // A newer click can replace the citation while the daemon is switching libraries
                let citation = citation_id.and_then(|id| {
                    page.pending_source.take_if(|op|
                    matches!(op, KnowledgeOperation::Read { id: pending, .. } if *pending == id)
                )
                });
                match result {
                    Ok(LibraryResponse::Command(KnowledgeResult::Status { status })) => {
                        this.apply_library_status(status, saved_form, cx);
                        if let Some(read) = citation {
                            this.library_action(read, cx);
                        }
                    }
                    Ok(LibraryResponse::Command(KnowledgeResult::CreatedDocument {
                        id,
                        path,
                        revision,
                    })) => {
                        page.location = Some(PreviewLocation {
                            id,
                            path,
                            revision: revision.clone(),
                            changed: false,
                            document: DocumentPreview::new(PreparedDocument::new("", 0), cx),
                        });
                        page.edit_conflicted = false;
                        page.edit_revision = Some(revision);
                        page.edit_original = "".into();
                        page.title_input.update(cx, |input, cx| {
                            input.clear(cx);
                            input.set_placeholder(tr!("knowledge.enter_title"), cx);
                        });
                        page.editor.update(cx, |input, cx| {
                            input.clear(cx);
                            input.set_placeholder(tr!("knowledge.start_writing"), cx);
                        });
                        page.panel = Panel::EditNew;
                        page.loaded = None;
                        let focus = page.title_input.read(cx).focus();
                        this.focus_library_input(focus, cx);
                        this.poll_library(cx);
                    }
                    Ok(LibraryResponse::Command(KnowledgeResult::Managed { paths })) => {
                        page.loaded = None;
                        page.move_picker = None;
                        page.panel = Panel::Preview;
                        if let Some((from, to)) = tree_move {
                            page.follow_move(&from, &to);
                        }
                        if let Some(id) = deleted {
                            if page.location.as_ref().is_some_and(|loc| loc.id == id) {
                                page.location = None;
                            }
                            let pin = this.state.pinned_documents.remove(&id);
                            let star = this.state.favorite_documents.remove(&id);
                            if pin || star {
                                this.save();
                            }
                            this.poll_library(cx);
                            cx.notify();
                            return;
                        }
                        if created_folder {
                            page.after_create = paths.into_iter().next();
                        } else if let Some(path) = managed_path {
                            if let Some(id) = page
                                .tree
                                .iter()
                                .find(|row| row.path == path)
                                .and_then(|r| r.id)
                            {
                                this.read_library_document(id, cx);
                            } else {
                                page.location = None;
                                page.after_create = Some(path);
                            }
                        }
                        this.poll_library(cx);
                    }
                    Ok(LibraryResponse::Command(KnowledgeResult::Versions { versions })) => {
                        page.versions = versions;
                        page.panel = Panel::History;
                    }
                    Ok(LibraryResponse::Command(KnowledgeResult::Source {
                        text,
                        revision,
                        ..
                    })) => {
                        page.edit_original = text.into();
                        page.editor.update(cx, |input, cx| {
                            input.set_placeholder("", cx);
                            input.set_content(page.edit_original.clone(), cx)
                        });
                        page.edit_conflicted = false;
                        page.edit_revision = Some(revision);
                        page.panel = Panel::Edit;
                    }
                    Ok(LibraryResponse::Preview(preview)) => {
                        let id = preview.id;
                        page.location = Some(preview.into_location(cx));
                        page.panel = Panel::Preview;
                        page.move_picker = None;
                        page.select_document(id);
                    }
                    Ok(_) => page.error = Some(tr!("assistant.invalid_response")),
                    Err(e) => page.error = Some(e.to_string()),
                }
                if catalog || rebuild {
                    this.poll_library(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

/// Build the complete hierarchy off the render thread so pagination never splits a folder in the UI
fn document_tree(
    docs: Vec<LibraryDocument>,
    folders: Vec<String>,
    positions: &HashMap<String, usize>,
    pins: &HashSet<Uuid>,
) -> Vec<TreeRow> {
    // Complete implicit parents for catalogs created before empty folders were tracked
    let mut directories = HashSet::new();
    for mut parent in folders.iter().map(String::as_str).chain(
        docs.iter()
            .map(|doc| row_actions::document_parts(&doc.path).0),
    ) {
        while !parent.is_empty() && directories.insert(parent.to_owned()) {
            parent = row_actions::document_parts(parent).0;
        }
    }
    let entries = directories
        .into_iter()
        .map(|path| (path, None, None))
        .chain(
            docs.into_iter()
                .map(|doc| (doc.path, Some(doc.id), doc.revision)),
        );
    let mut children: HashMap<String, Vec<TreeRow>> = HashMap::new();
    for (path, id, revision) in entries {
        let (parent, name) = row_actions::document_parts(&path);
        children
            .entry(parent.to_owned())
            .or_default()
            .push(TreeRow {
                name: name.to_owned(),
                path,
                id,
                revision,
                depth: 0,
                can_move_up: false,
                can_move_down: false,
            });
    }
    for rows in children.values_mut() {
        rows.sort_unstable_by(|a, b| {
            let key = |r: &TreeRow| {
                (
                    !r.id.is_some_and(|id| pins.contains(&id)),
                    positions.get(&r.path).copied().unwrap_or(usize::MAX),
                    r.id.is_some(),
                )
            };
            key(a).cmp(&key(b)).then_with(|| a.name.cmp(&b.name))
        });
        let pinned_count = rows.partition_point(|r| r.id.is_some_and(|id| pins.contains(&id)));
        for section in [0..pinned_count, pinned_count..rows.len()] {
            let count = section.len();
            for (i, row) in rows[section].iter_mut().enumerate() {
                row.can_move_up = i > 0;
                row.can_move_down = i + 1 < count;
            }
        }
    }
    let mut rows = Vec::new();
    let mut pending: Vec<_> = children
        .remove("")
        .unwrap_or_default()
        .into_iter()
        .rev()
        .collect();
    while let Some(row) = pending.pop() {
        if let Some(items) = children.remove(&row.path) {
            pending.extend(items.into_iter().rev().map(|mut child| {
                child.depth = row.depth + 1;
                child
            }));
        }
        rows.push(row);
    }
    rows
}

impl KnowledgePage {
    fn refresh_visible(&mut self) {
        self.visible.clear();
        let mut closed = None;
        for (i, row) in self.tree.iter().enumerate() {
            if closed.is_some_and(|depth| row.depth > depth) {
                continue;
            }
            self.visible.push(i);
            closed = (row.id.is_none() && self.collapsed.contains(&row.path)).then_some(row.depth);
        }
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }
    fn select_document(&mut self, id: Uuid) {
        if let Some(i) = self.tree.iter().position(|r| r.id == Some(id)) {
            self.reveal_tree_row(i);
        }
    }

    /// Expand collapsed ancestors before scrolling a newly created folder or document into view
    fn reveal_tree_row(&mut self, i: usize) {
        let path = self.tree[i].path.replace('\\', "/");
        self.collapsed
            .retain(|p| *p == path || !Path::new(&path).starts_with(p));
        self.refresh_visible();
        if let Some(pos) = self.visible.iter().position(|r| *r == i) {
            self.cursor = pos;
            self.tree_scroll
                .scroll_to_item(pos, gpui::ScrollStrategy::Nearest);
        }
    }
}

impl Waku {
    /// Mount the fresh editor or directory row before sending keyboard input to it
    fn focus_library_input(&self, focus: FocusHandle, cx: &mut Context<Self>) {
        if self.knowledge.open && self.settings_page.is_none() {
            let _ = self.knowledge.window.update(cx, |_, window, _| {
                window.on_next_frame(move |window, cx| window.focus(&focus, cx));
            });
        }
    }

    fn library_visible(&self) -> bool {
        self.settings_page == Some(SettingsPage::Knowledge)
            || (self.settings_page.is_none() && self.knowledge.open)
    }
    pub(super) fn library_name(&self) -> String {
        self.knowledge
            .status
            .config
            .as_ref()
            .map(|c| library_folder_name(&c.root).to_owned())
            .unwrap_or_else(|| tr!("knowledge.title"))
    }
    fn read_library_document(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.library_action(
            KnowledgeOperation::Read {
                id,
                revision: None,
                start: 0,
                end: None,
            },
            cx,
        );
    }
    /// Resolve an internal citation on the daemon before displaying any note text
    pub(super) fn open_library_citation(&mut self, target: &str, cx: &mut Context<Self>) -> bool {
        let Some(op) = waku_protocol::knowledge::citation_operation(target) else {
            return false;
        };
        if self.has_library_draft(cx) {
            return true;
        }
        self.leave_translation();
        self.settings_page = None;
        self.knowledge.open = true;
        self.knowledge.pending_source = Some(op);
        self.poll_library(cx);
        cx.notify();
        true
    }

    fn select_tree_row(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let p = &mut self.knowledge;
        let Some(&index) = p.visible.get(pos) else {
            return;
        };
        p.cursor = pos;
        let row = &p.tree[index];
        let id = row.id;
        window.focus(&p.tree_focus, cx);
        if let Some(id) = id {
            if !self.has_library_draft(cx) {
                self.read_library_document(id, cx);
            }
        } else {
            let path = row.path.clone();
            if !p.collapsed.remove(&path) {
                p.collapsed.insert(path);
            }
            p.refresh_visible();
            cx.notify();
        }
    }

    fn tree_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let p = &mut self.knowledge;
        if p.visible.is_empty() || p.rename_input.read(cx).focus().is_focused(window) {
            return;
        }
        let row = &p.tree[p.visible[p.cursor]];
        match event.keystroke.key.as_str() {
            "up" => p.cursor = p.cursor.saturating_sub(1),
            "down" => p.cursor = (p.cursor + 1).min(p.visible.len() - 1),
            "home" => p.cursor = 0,
            "end" => p.cursor = p.visible.len() - 1,
            "left" if row.id.is_none() && !p.collapsed.contains(&row.path) => {
                p.collapsed.insert(row.path.clone());
                p.refresh_visible();
            }
            "left" => {
                if let Some(parent) = p.visible[..p.cursor]
                    .iter()
                    .rposition(|i| p.tree[*i].depth < row.depth)
                {
                    p.cursor = parent;
                }
            }
            "right" if row.id.is_none() => {
                if !p.collapsed.remove(&row.path) {
                    p.cursor = (p.cursor + 1).min(p.visible.len() - 1);
                }
                p.refresh_visible();
            }
            "enter" | "space" => {
                let pos = p.cursor;
                self.select_tree_row(pos, window, cx);
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        p.tree_scroll
            .scroll_to_item(p.cursor, gpui::ScrollStrategy::Nearest);
        cx.stop_propagation();
        cx.notify();
    }

    /// Back only abandons a dirty form after the user confirms for this exact edit
    fn leave_library_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.knowledge.busy {
            return;
        }
        if !self.knowledge.has_draft(cx) {
            self.library_mode(Panel::Preview, cx);
            return;
        }
        let revision = self.knowledge.revision;
        let confirm = window.prompt(
            gpui::PromptLevel::Warning,
            &tr!("knowledge.confirm_discard"),
            None,
            &[
                gpui::PromptButton::cancel(tr!("knowledge.keep_editing")),
                gpui::PromptButton::new(tr!("knowledge.discard")),
            ],
            cx,
        );
        cx.spawn(async move |view, cx| {
            if confirm.await == Ok(1) {
                let _ = view.update(cx, |this, cx| {
                    if this.knowledge.revision == revision {
                        this.library_mode(Panel::Preview, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn library_mode(&mut self, mode: Panel, cx: &mut Context<Self>) {
        if self.knowledge.busy || (mode != Panel::Preview && self.has_library_draft(cx)) {
            return;
        }
        self.knowledge.revision += 1;
        self.knowledge.error = None;
        self.knowledge.after_create = None;
        if mode == Panel::Import {
            self.knowledge.path_input.update(cx, |i, cx| {
                i.set_placeholder(tr!("knowledge.note_path"), cx);
                i.clear(cx);
            });
            self.knowledge.url_input.update(cx, |i, cx| i.clear(cx));
        }
        // Returning to a preview reconciles any edits or deletion observed while the form was open
        if mode == Panel::Preview {
            self.knowledge.loaded = None;
        }
        self.knowledge.panel = mode;
        self.knowledge.move_picker = None;
        cx.notify();
    }

    /// Both the root menu and a folder menu allocate the default name on the daemon
    fn create_library_entry(&mut self, parent: &str, folder: bool, cx: &mut Context<Self>) {
        if !self.tree_editable(cx) {
            return;
        }
        let Some(config) = &self.knowledge.status.config else {
            return;
        };
        self.library_action(
            KnowledgeOperation::CreateUntitled {
                root: config.root.clone(),
                parent: parent.into(),
                stem: tr!(if folder {
                    "knowledge.untitled_folder"
                } else {
                    "knowledge.untitled_document"
                }),
                folder,
            },
            cx,
        );
    }

    fn save_library_form(&mut self, cx: &mut Context<Self>) {
        if self.knowledge.busy {
            return;
        }
        let p = &self.knowledge;
        let operation = match p.panel {
            Panel::Import => KnowledgeOperation::Manage {
                action: LibraryAction::ImportUrl {
                    path: p.path_input.read(cx).content().trim().into(),
                    url: p.url_input.read(cx).content().trim().into(),
                    options: Default::default(),
                },
            },
            Panel::Edit | Panel::EditNew => {
                let Some(loc) = &p.location else { return };
                let Some(config) = &p.status.config else {
                    return;
                };
                KnowledgeOperation::ManageTree {
                    root: config.root.clone(),
                    id: Some(loc.id),
                    action: LibraryAction::Update {
                        path: loc.path.clone(),
                        content: if p.panel == Panel::EditNew {
                            new_note_markdown(
                                p.title_input.read(cx).content(),
                                p.editor.read(cx).content(),
                            )
                        } else {
                            p.editor.read(cx).content().into()
                        },
                        revision: p.edit_revision.clone(),
                        mode: UpdateMode::Replace,
                    },
                }
            }
            _ => return,
        };
        self.library_action(operation, cx);
    }

    /// Start a manual index rebuild from the knowledge settings
    fn library_reindex_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        assistant_button_base("library-reindex", tr!("knowledge.rebuild"), theme).when_else(
            self.knowledge.busy,
            |button| button.opacity(0.5),
            |button| {
                button
                    .hover(|style| style.bg(theme.overlay))
                    .on_activation(cx, |this, _, cx| {
                        if !this.knowledge.busy && !this.knowledge.status.indexing {
                            this.library_action(KnowledgeOperation::Rebuild, cx);
                        }
                    })
            },
        )
    }

    pub(super) fn render_library_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let menu = self.menu_handle("library-menu", cx);
        let name = self.library_name();
        let trigger = MenuChip::new("library-name")
            .label(name)
            .selected(menu.is_open());
        let weak = cx.weak_entity();
        dropdown_menu(
            trigger,
            "library-menu",
            &menu,
            MenuAlign::BelowLeft,
            move |cx| {
                let weak = weak.clone();
                let mut items = weak
                    .read_with(cx, |t, _| {
                        t.knowledge
                            .status
                            .libraries
                            .iter()
                            .map(|library| {
                                let root = library.root.clone();
                                let active = t
                                    .knowledge
                                    .status
                                    .config
                                    .as_ref()
                                    .is_some_and(|c| c.root == root);
                                let weak = weak.clone();
                                let name = library_folder_name(&root);
                                let label = format!("{name}  ·  {root}");
                                MenuItem::new(label, move |_, cx| {
                                    let _ = weak.update(cx, |t, cx| {
                                        t.library_action(
                                            KnowledgeOperation::ActivateLibrary {
                                                root: root.clone(),
                                            },
                                            cx,
                                        );
                                    });
                                })
                                .selected(active)
                                .disabled(active)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !items.is_empty() {
                    items.push(MenuItem::Separator);
                }
                items.push(MenuItem::new(tr!("knowledge.configure"), move |_, cx| {
                    let _ = weak.update(cx, |t, cx| {
                        t.open_settings_page(SettingsPage::Knowledge, cx)
                    });
                }));
                items
            },
        )
        .into_any_element()
    }

    /// Keep creation and import actions beside the directory heading
    fn library_directory_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let menu = self.menu_handle("library-directory-actions", cx);
        let weak = cx.weak_entity();
        dropdown_menu(
            icon_button("library-add", "icons/plus.svg", theme)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .when(menu.is_open(), |el| el.bg(theme.overlay_strong))
                .tooltip(Tooltip::text(tr!("knowledge.add"))),
            "library-directory-actions",
            &menu,
            MenuAlign::BelowLeft,
            move |_| {
                let create = weak.clone();
                let folder = weak.clone();
                let file = weak.clone();
                let url = weak.clone();
                vec![
                    MenuItem::new(tr!("knowledge.new"), move |_, cx| {
                        let _ =
                            create.update(cx, |this, cx| this.create_library_entry("", false, cx));
                    })
                    .icon("icons/file.svg"),
                    MenuItem::new(tr!("knowledge.new_folder"), move |_, cx| {
                        let _ =
                            folder.update(cx, |this, cx| this.create_library_entry("", true, cx));
                    })
                    .icon("icons/folder.svg"),
                    MenuItem::Separator,
                    MenuItem::new(tr!("knowledge.import_document"), move |_, cx| {
                        let _ = file.update(cx, |this, cx| this.import_local_document(cx));
                    })
                    .icon("icons/file-bottom-left-arrow.svg"),
                    MenuItem::new(tr!("knowledge.import"), move |_, cx| {
                        let _ = url.update(cx, |this, cx| this.library_mode(Panel::Import, cx));
                    })
                    .icon("icons/globe.svg"),
                ]
            },
        )
    }

    /// The overflow menu belongs to the document currently shown in the preview
    fn library_document_menu(&self, path: String, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let menu = self.menu_handle("library-document-actions", cx);
        let weak = cx.weak_entity();
        dropdown_menu(
            icon_button("library-document-more", "icons/ellipsis.svg", theme)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .when(menu.is_open(), |el| el.bg(theme.overlay_strong))
                .tooltip(Tooltip::text(tr!("knowledge.document_actions"))),
            "library-document-actions",
            &menu,
            MenuAlign::BelowRight,
            move |_| {
                let item = |label, icon, op: KnowledgeOperation| {
                    let weak = weak.clone();
                    MenuItem::new(label, move |_, cx| {
                        let _ = weak.update(cx, |this, cx| this.library_action(op.clone(), cx));
                    })
                    .icon(icon)
                };
                vec![
                    item(
                        tr!("knowledge.edit"),
                        "icons/pencil.svg",
                        KnowledgeOperation::Source { path: path.clone() },
                    ),
                    MenuItem::Separator,
                    item(
                        tr!("knowledge.history"),
                        "icons/rewind.svg",
                        KnowledgeOperation::History { path: path.clone() },
                    ),
                ]
            },
        )
    }

    pub(super) fn render_knowledge_workspace(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.knowledge;
        let theme = Theme::current(cx);
        let tree = gpui::uniform_list(
            "library-tree",
            p.visible.len(),
            cx.processor(|this, range: Range<usize>, window, cx| {
                let tree_focused = this.knowledge.tree_focus.is_focused(window);
                let keyboard_navigation = window.last_input_was_keyboard();
                range
                    .map(|pos| {
                        let p = &this.knowledge;
                        let r = &p.tree[p.visible[pos]];
                        let is_dir = r.id.is_none();
                        let expanded = is_dir && !p.collapsed.contains(&r.path);
                        let chosen = r.id.is_some() && p.location.as_ref().map(|l| l.id) == r.id;
                        let cursor = tree_focused && pos == p.cursor;
                        let renaming = p
                            .rename_target
                            .as_ref()
                            .is_some_and(|target| target.path == r.path && target.id == r.id);
                        let theme = Theme::current(cx);
                        let group: SharedString = format!("library-row-{pos}").into();
                        let label = if renaming {
                            TextField::new("knowledge-rename-field", p.rename_input.clone())
                                .flex_1()
                                .min_w_0()
                                .on_mouse_down_out(cx.listener(Self::click_outside_document_rename))
                                .on_action(cx.listener(
                                    |this, _: &crate::input::Clear, window, cx| {
                                        this.focus_knowledge(window, cx);
                                    },
                                ))
                                .into_any_element()
                        } else {
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(r.name.clone())
                                .into_any_element()
                        };
                        div()
                            .id(("knowledge-entry", pos))
                            .group(group.clone())
                            .h(px(29.))
                            .w_full()
                            .pl(px(8. + 14. * r.depth.min(12) as f32))
                            .pr(px(6.))
                            .flex()
                            .gap(px(5.))
                            .items_center()
                            .rounded(px(5.))
                            .cursor_default()
                            // A filled row marks selection, with heavier text for the keyboard cursor
                            .when(chosen || cursor, |e| e.bg(theme.sidebar_item_background))
                            .when(cursor && keyboard_navigation, |e| {
                                e.bg(theme.overlay_strong).font_weight(FontWeight::SEMIBOLD)
                            })
                            .when(!chosen && !cursor, |e| e.hover(|s| s.bg(theme.overlay)))
                            .child(div().w(px(10.)).flex_none().when(is_dir, |e| {
                                e.child(icon(
                                    if expanded {
                                        "icons/chevron-down.svg"
                                    } else {
                                        "icons/chevron-right.svg"
                                    },
                                    10.,
                                    theme.text_tertiary,
                                ))
                            }))
                            .child(icon(
                                if is_dir {
                                    if expanded {
                                        "icons/folder-open.svg"
                                    } else {
                                        "icons/folder.svg"
                                    }
                                } else {
                                    "icons/file.svg"
                                },
                                14.,
                                theme.text_secondary,
                            ))
                            .child(label)
                            .when_some(r.id, |row, id| {
                                row.when(this.state.favorite_documents.contains(&id), |e| {
                                    e.child(icon(
                                        "icons/star-filled.svg",
                                        12.,
                                        theme.text_secondary,
                                    ))
                                })
                            })
                            .when(!renaming, |row| {
                                let item =
                                    TreeItem::new(r, &p.status.config.as_ref().unwrap().root);
                                let row = this.with_library_row_menu(
                                    row, &item, r, group, chosen, window, cx,
                                );
                                this.tree_drag_row(row, item, cx).on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.select_tree_row(pos, window, cx)
                                    },
                                ))
                            })
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&p.tree_scroll)
        .flex_1()
        .min_h_0();
        let left =
            div()
                .w(gpui::relative(0.30))
                .min_w(px(140.))
                .max_w(px(280.))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(theme.border)
                .bg(theme.canvas)
                .child(
                    div()
                        .id("library-root-drop")
                        .drag_over::<TreeItem>(move |s, _, _, _| s.bg(theme.overlay_strong))
                        .on_drop(cx.listener(|this, item: &TreeItem, _, cx| {
                            this.move_tree_item(item, "", cx)
                        }))
                        .h(px(36.))
                        .px(px(12.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(4.))
                        .text_color(theme.text_secondary)
                        .child(tr!("knowledge.tree"))
                        .when(p.status.config.is_some(), |row| {
                            row.child(self.library_directory_menu(cx))
                        }),
                )
                .child(
                    div()
                        .id("library-tree-focus")
                        .track_focus(&p.tree_focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .flex_1()
                        .min_h_0()
                        .px(px(5.))
                        .flex()
                        .flex_col()
                        .on_key_down(cx.listener(Self::tree_key))
                        .child(tree),
                );
        let status = &p.status;
        let working = p.busy || p.rename_save.is_some();
        let footer = div()
            .h(px(32.))
            .flex_none()
            .border_t_1()
            .border_color(theme.border)
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .text_color(theme.text_secondary)
            .when(status.indexing, |row| {
                let percent = status
                    .ready
                    .saturating_mul(100)
                    .checked_div(status.total)
                    .unwrap_or(0)
                    .min(100);
                row.child(tr!(
                    "knowledge.indexing",
                    ready = status.ready,
                    total = status.total
                ))
                .child(
                    div()
                        .w(px(76.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(theme.overlay_strong)
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .w(gpui::relative(percent as f32 / 100.))
                                .bg(theme.accent),
                        ),
                )
                .child(format!("{percent}%"))
            })
            .when(working, |e| e.child(tr!("knowledge.working")));
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .text_size(sp(12.5))
            .text_color(theme.text)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(left)
                    .child(self.library_detail(cx)),
            )
            .children(
                p.error
                    .as_ref()
                    .or(p.poll_error.as_ref())
                    .or(status.error.as_ref())
                    .map(|e| {
                        div()
                            .px(px(12.))
                            .py(px(5.))
                            .text_color(theme.danger)
                            .child(e.clone())
                    }),
            )
            .when(
                status.config.is_some() && (status.indexing || working),
                |pane| pane.child(footer),
            )
            .into_any_element()
    }

    fn library_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.knowledge;
        let theme = Theme::current(cx);
        let base = div()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme.surface);
        if p.status.config.is_none() {
            return base.into_any_element();
        }
        if p.panel == Panel::Move
            && let Some(picker) = &p.move_picker
        {
            return self.render_move_picker(base, picker, cx);
        }
        if matches!(p.panel, Panel::EditNew | Panel::Edit | Panel::Import) {
            let mut editor = base.p(px(14.)).gap(px(8.));
            if p.panel == Panel::Import {
                editor = editor
                    .child(tr!("knowledge.import"))
                    .child(TextField::new("note-path", p.path_input.clone()).w_full())
                    .child(TextField::new("article-url", p.url_input.clone()).w_full());
            } else {
                if p.panel == Panel::EditNew {
                    editor = editor.px(px(30.)).pt(px(28.)).gap(px(16.)).child(
                        div()
                            .text_size(sp(28.))
                            .line_height(sp(38.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(p.title_input.clone()),
                    );
                } else {
                    editor = editor.child(tr!("knowledge.edit"));
                }
                if p.edit_conflicted {
                    editor = editor.child(tr!("knowledge.edit_conflict"));
                }
                editor = editor.child(
                    div()
                        .id("note-editor")
                        .on_click(cx.listener(|this, _, window, cx| {
                            window.focus(&this.knowledge.editor.read(cx).focus(), cx);
                        }))
                        .when(p.panel == Panel::Edit, |el| {
                            el.border_1()
                                .border_color(theme.border)
                                .rounded(px(6.))
                                .p(px(10.))
                        })
                        .overflow_y_scroll()
                        .flex_1()
                        .min_h_0()
                        .child(p.editor.clone()),
                );
            }
            return editor
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(assistant_button(
                            "save-note",
                            tr!("knowledge.save"),
                            cx,
                            |t, _, cx| t.save_library_form(cx),
                        ))
                        .child(assistant_button(
                            "cancel-note",
                            tr!("settings.back"),
                            cx,
                            |t, window, cx| t.leave_library_form(window, cx),
                        )),
                )
                .into_any_element();
        }
        let Some(loc) = &p.location else {
            return base.into_any_element();
        };
        if p.panel == Panel::History {
            let mut history = base
                .id("note-versions")
                .overflow_y_scroll()
                .p(px(12.))
                .gap(px(6.))
                .child(tr!("knowledge.history"))
                .child(assistant_button(
                    "leave-history",
                    tr!("settings.back"),
                    cx,
                    |t, _, cx| t.library_mode(Panel::Preview, cx),
                ));
            if p.versions.is_empty() {
                history = history.child(tr!("knowledge.no_versions"));
            }
            for (i, v) in p.versions.iter().enumerate() {
                let version = v.version.clone();
                let path = loc.path.clone();
                history = history.child(assistant_button(
                    ("restore-note", i),
                    tr!("knowledge.restore", date = v.saved_at.as_str()),
                    cx,
                    move |t, _, cx| {
                        t.library_action(
                            KnowledgeOperation::Manage {
                                action: LibraryAction::Restore {
                                    path: path.clone(),
                                    version: version.clone(),
                                },
                            },
                            cx,
                        )
                    },
                ));
            }
            return history.into_any_element();
        }
        let mut body = base.child(
            div()
                .h(px(36.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(6.))
                .child(icon("icons/file.svg", 14., theme.text_tertiary))
                .child(div().flex_1().min_w_0().truncate().child(loc.path.clone()))
                .child(self.library_document_menu(loc.path.clone(), cx)),
        );
        if loc.changed {
            body = body.child(tr!("knowledge.changed"));
        }
        body.child(loc.document.render(
            MarkdownMetrics::document(self.state.ui_font_size, self.state.code_font_size),
            self.state.render_math,
            cx,
        ))
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skipping either input must not inject an empty heading or strip Markdown whitespace
    #[test]
    fn new_document_title_and_body_are_optional() {
        assert_eq!(new_note_markdown("", ""), "");
        assert_eq!(new_note_markdown(" 标题 ", ""), "# 标题");
        assert_eq!(new_note_markdown("", "    code\n"), "    code\n");
        assert_eq!(new_note_markdown("予定", "明日\n"), "# 予定\n\n明日\n");
    }

    /// Pinning promotes documents within their own directory while preserving the hierarchy
    #[test]
    fn document_marks_order_pinned_siblings() {
        let docs: Vec<_> = ["a.md", "folder/a.md", "folder/z.md", "z.md"]
            .into_iter()
            .map(|path| LibraryDocument {
                id: Uuid::new_v4(),
                path: path.into(),
                revision: None,
            })
            .collect();
        let pins = HashSet::from([docs[2].id, docs[3].id]);
        let rows = document_tree(
            docs,
            vec!["folder/empty".into(), "folder".into()],
            &HashMap::new(),
            &pins,
        );
        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            [
                "z.md",
                "folder",
                "folder/z.md",
                "folder/empty",
                "folder/a.md",
                "a.md"
            ]
        );
    }

    /// Manual positions mix folders and files within a parent while pins retain their priority
    #[test]
    fn manual_tree_order_keeps_pins_and_implicit_parents() {
        let docs: Vec<_> = ["a.md", "pin.md", "folder/z.md", "folder/a.md"]
            .into_iter()
            .map(|path| LibraryDocument {
                id: Uuid::new_v4(),
                path: path.into(),
                revision: None,
            })
            .collect();
        let pins = HashSet::from([docs[1].id]);
        let positions = HashMap::from([
            ("a.md".into(), 0),
            ("folder".into(), 1),
            ("folder/z.md".into(), 0),
        ]);
        let rows = document_tree(docs, vec!["folder/empty/child".into()], &positions, &pins);
        assert_eq!(
            rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                "pin.md",
                "a.md",
                "folder",
                "folder/z.md",
                "folder/empty",
                "folder/empty/child",
                "folder/a.md"
            ]
        );
    }

    /// Local import keeps the source bytes and sends only a filename to the daemon
    #[test]
    fn local_document_import_preserves_name_and_text() -> anyhow::Result<()> {
        let directory = std::env::temp_dir().join(format!("waku-import-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory)?;
        let name = "会议纪要.MD";
        let source = directory.join(name);
        let text = "---\r\ntags: [会议]\r\n---\r\n# 纪要\r\n";
        std::fs::write(&source, text)?;
        let action = local_document_action(&source)?;
        assert!(
            matches!(action, LibraryAction::Create { path, content, metadata: None, format: None }
            if path == name && content == text)
        );
        let unsupported = directory.join("会议纪要.pdf");
        std::fs::rename(&source, &unsupported)?;
        assert!(local_document_action(&unsupported).is_err());
        std::fs::remove_file(unsupported)?;
        std::fs::remove_dir(directory)?;
        Ok(())
    }
}
