//! Library-scoped note mutations, revision history and immediately consistent catalog updates

use super::{KnowledgeLibrary, index, markdown};
use anyhow::{Context, Result, ensure};
use cap_std::fs::{Dir, OpenOptions};
use rusqlite::{Connection, params};
use std::{
    collections::HashSet,
    io::{ErrorKind, Read, Write},
    path::{Component, Path},
};
use uuid::Uuid;
use waku_protocol::knowledge::*;

/// Notes and folders use visible components relative to the selected library
pub(super) fn relative_path(path: &str) -> Result<&Path> {
    let p = Path::new(path);
    ensure!(
        path.split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.'))
            && !path.contains('\\')
            && p.components().all(
                |c| matches!(c,Component::Normal(name) if !name.to_string_lossy().starts_with('.'))
            ),
        "Expected a visible library-relative path"
    );
    Ok(p)
}

pub(super) fn valid_path(path: &str) -> Result<&Path> {
    let p = relative_path(path)?;
    ensure!(
        index::is_document(p),
        "Expected a relative .md, .markdown or .xml document path"
    );
    Ok(p)
}

/// Anchor every operation in a directory capability, including if another process replaces a parent
pub(super) fn root_dir(root: &Path) -> Result<Dir> {
    ensure!(root.canonicalize()? == root, "Library root moved");
    Ok(Dir::open_ambient_dir(root, cap_std::ambient_authority())?)
}

/// Match the scanner's exclusion of symlinks for existing and new paths
pub(super) fn check_ancestors(dir: &Dir, path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match dir.symlink_metadata(ancestor) {
            Ok(meta) => ensure!(
                !meta.is_symlink(),
                "Symbolic links are excluded from the library"
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn open_note(dir: &Dir, path: &Path) -> Result<Option<cap_std::fs::File>> {
    check_ancestors(dir, path)?;
    let file = match dir.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= index::MAX_NOTE_BYTES,
        "Expected a regular document of at most 32 MiB"
    );
    Ok(Some(file))
}

/// Restrict listing I/O to a small header even when an entire folder contains large documents
pub(super) fn entry_details(dir: &Dir, path: &str) -> Result<Option<serde_json::Value>> {
    let relative = valid_path(path)?;
    let Some(file) = open_note(dir, relative)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    let mut header = Vec::new();
    file.take(8192).read_to_end(&mut header)?;
    let header = String::from_utf8_lossy(&header);
    let (fields, body) =
        markdown::front_matter(&header).unwrap_or_else(|_| (Default::default(), header.as_ref()));
    let title = fields
        .get("title")
        .and_then(|v| v.as_str())
        .or_else(|| body.lines().find_map(|line| line.strip_prefix("# ")))
        .or_else(|| relative.file_stem()?.to_str());
    let modified: chrono::DateTime<chrono::Utc> = metadata.modified()?.into_std().into();
    Ok(Some(serde_json::json!({
        "path":path, "directory":false, "title":title, "tags":fields.get("tags"), "size":metadata.len(),
        "modified":modified.to_rfc3339(), "type":if index::is_markdown(relative){"markdown"}else{"richtext"}
    })))
}

fn current(dir: &Dir, path: &Path) -> Result<Option<String>> {
    let Some(mut file) = open_note(dir, path)? else {
        return Ok(None);
    };
    let mut text = String::new();
    (&mut file)
        .take(index::MAX_NOTE_BYTES + 1)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= index::MAX_NOTE_BYTES,
        "Note exceeds 32 MiB"
    );
    Ok(Some(text))
}

pub(super) fn read_note(root: &Path, path: &str) -> Result<String> {
    current(&root_dir(root)?, valid_path(path)?)?.context("Document does not exist")
}

/// Stage beside the destination and publish without overwriting a concurrent create
pub(super) fn atomic_write(dir: &Dir, path: &Path, data: &[u8], replace: bool) -> Result<()> {
    ensure!(
        data.len() as u64 <= index::MAX_NOTE_BYTES,
        "Note exceeds 32 MiB"
    );
    let parent = path.parent().context("Missing destination directory")?;
    if !parent.as_os_str().is_empty() {
        dir.create_dir_all(parent)?;
    }
    let tmp = parent.join(format!(".waku-{}", Uuid::new_v4()));
    let mut file = dir.open_with(&tmp, OpenOptions::new().write(true).create_new(true))?;
    let result = (|| -> Result<()> {
        if replace {
            file.set_permissions(dir.metadata(path)?.permissions())?;
        }
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        if replace {
            dir.rename(&tmp, dir, path)?;
        } else {
            dir.hard_link(&tmp, dir, path)?;
        }
        Ok(())
    })();
    let _ = dir.remove_file(tmp);
    result
}

/// Validate new document bytes on both writes and renames that change the file format
pub(super) fn validate_body(path: &Path, body: &str) -> Result<()> {
    ensure!(
        body.len() as u64 <= index::MAX_NOTE_BYTES,
        "Note exceeds 32 MiB"
    );
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
    {
        let mut xml = quick_xml::Reader::from_str(body);
        while !matches!(xml.read_event()?, quick_xml::events::Event::Eof) {}
    }
    Ok(())
}

/// The versions table belongs to the history database, either opened directly or attached for a move
pub(super) fn save_version(db: &Connection, root: &str, path: &str, body: &str) -> Result<()> {
    db.execute(
        "INSERT INTO versions VALUES(?,?,?,?,?)",
        params![
            Uuid::new_v4().to_string(),
            root,
            path,
            chrono::Utc::now().to_rfc3339(),
            body
        ],
    )?;
    db.execute(
        "DELETE FROM versions WHERE root=?1 AND path=?2 AND version NOT IN
        (SELECT version FROM versions WHERE root=?1 AND path=?2 ORDER BY saved_at DESC LIMIT 50)",
        [root, path],
    )?;
    Ok(())
}

struct Change {
    path: String,
    before: Option<String>,
    after: Option<String>,
}

impl Change {
    fn edit(path: String, before: String, after: String) -> Self {
        Self {
            path,
            before: Some(before),
            after: Some(after),
        }
    }
    fn create(path: String, after: String) -> Self {
        Self {
            path,
            before: None,
            after: Some(after),
        }
    }
}

impl KnowledgeLibrary {
    /// Historical note text lives outside the rebuildable vector index
    pub(super) fn history_db(&self) -> Result<Connection> {
        let db = index::connect(&self.path.with_file_name("knowledge-history.sqlite"))?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS versions(version TEXT PRIMARY KEY,root TEXT NOT NULL,path TEXT NOT NULL,saved_at TEXT NOT NULL,body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS note_versions ON versions(root,path,saved_at)")?;
        Ok(db)
    }

    pub(super) fn versions(&self, root: &str, path: &str) -> Result<Vec<LibraryVersion>> {
        valid_path(path)?;
        let db = self.history_db()?;
        let mut stmt=db.prepare("SELECT version,saved_at FROM versions WHERE root=? AND path=? ORDER BY saved_at DESC LIMIT 50")?;
        Ok(stmt
            .query_map([root, path], |r| {
                Ok(LibraryVersion {
                    version: r.get(0)?,
                    saved_at: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(super) fn manage(&self, action: LibraryAction) -> Result<KnowledgeResult> {
        let (generation, config) = self.active()?;
        // Fetching an article must not stall another assistant's local notes or the configuration UI
        let article = match &action {
            LibraryAction::ImportUrl { url, options, .. } => {
                Some(super::import::article(url, options)?)
            }
            _ => None,
        };
        let _serial = self.writes.lock();
        self.current(generation)?;
        self.apply_action(config, action, article)
    }

    /// Allocate and publish the name under the same lock so simultaneous creates get separate entries
    pub(super) fn create_untitled(
        &self,
        root: &str,
        parent: &str,
        stem: &str,
        folder: bool,
    ) -> Result<KnowledgeResult> {
        let _serial = self.writes.lock();
        let (_, config) = self.active()?;
        ensure!(config.root == root, "The active knowledge library changed");
        ensure!(
            relative_path(stem)?.components().count() == 1,
            "Expected a filename"
        );
        let dir = root_dir(Path::new(root))?;
        if !parent.is_empty() {
            let parent = relative_path(parent)?;
            check_ancestors(&dir, parent)?;
            ensure!(dir.is_dir(parent), "Destination folder no longer exists");
        }
        let extension = if folder { "" } else { ".md" };
        for number in 1u64.. {
            let name = if number == 1 {
                format!("{stem}{extension}")
            } else {
                format!("{stem} {number}{extension}")
            };
            let path = if parent.is_empty() {
                name
            } else {
                format!("{parent}/{name}")
            };
            match dir.symlink_metadata(&path) {
                Ok(_) => continue,
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if folder {
                return self.apply_action(config, LibraryAction::CreateFolder { path }, None);
            }
            self.apply_action(
                config,
                LibraryAction::Create {
                    path: path.clone(),
                    content: String::new(),
                    metadata: None,
                    format: None,
                },
                None,
            )?;
            return Ok(KnowledgeResult::CreatedDocument {
                id: self.document_id(&path)?,
                path,
                revision: index::revision(""),
            });
        }
        anyhow::bail!("No available filename")
    }

    /// Validate the menu target under the same lock as the filesystem and catalog changes
    pub(super) fn manage_tree(
        &self,
        root: &str,
        id: Option<Uuid>,
        action: LibraryAction,
    ) -> Result<KnowledgeResult> {
        let _serial = self.writes.lock();
        let (_, config) = self.active()?;
        ensure!(config.root == root, "The active knowledge library changed");
        let path = match &action {
            LibraryAction::Move { from, to } => {
                let parent = relative_path(to)?.parent().unwrap();
                ensure!(
                    parent.as_os_str().is_empty() || root_dir(Path::new(root))?.is_dir(parent),
                    "Destination folder no longer exists"
                );
                from
            }
            LibraryAction::Reorder { path, .. } | LibraryAction::DeleteFolder { path } => path,
            LibraryAction::Update { path, .. } if id.is_some() => path,
            _ => anyhow::bail!("Expected a directory operation"),
        };
        if let Some(id) = id {
            let location = index::document_location(&index::connect(&self.path)?, id)?;
            ensure!(
                location.0 == root && location.1 == *path,
                "The document has moved"
            );
        } else {
            ensure!(
                root_dir(Path::new(root))?.is_dir(relative_path(path)?),
                "Folder no longer exists"
            );
        }
        self.apply_action(config, action, None)
    }

    /// Resolve the row under the write lock so a stale menu cannot act on a replacement file
    pub(super) fn document_file(
        &self,
        id: Uuid,
        action: DocumentFileAction,
    ) -> Result<KnowledgeResult> {
        let _serial = self.writes.lock();
        let (_, config) = self.active()?;
        let path: String = index::connect(&self.path)?
            .query_row(
                "SELECT path FROM documents WHERE root=? AND id=?",
                params![config.root, id.to_string()],
                |row| row.get(0),
            )
            .context("This document is no longer in the library")?;
        let action = match action {
            DocumentFileAction::Source => {
                let text = read_note(Path::new(&config.root), &path)?;
                return Ok(KnowledgeResult::Source {
                    path,
                    revision: index::revision(&text),
                    text,
                });
            }
            DocumentFileAction::Duplicate { to } => LibraryAction::Create {
                content: read_note(Path::new(&config.root), &path)?,
                path: to,
                metadata: None,
                format: None,
            },
            DocumentFileAction::Move { to } => LibraryAction::Move { from: path, to },
            DocumentFileAction::Delete => LibraryAction::Delete { path },
        };
        self.apply_action(config, action, None)
    }

    /// The caller holds the write lock through the file, history and catalog updates
    fn apply_action(
        &self,
        config: LibraryConfig,
        action: LibraryAction,
        article: Option<super::import::Article>,
    ) -> Result<KnowledgeResult> {
        let root = Path::new(&config.root);
        let dir = root_dir(root)?;
        let read =
            |path: &str| current(&dir, valid_path(path)?)?.context("Document does not exist");
        let mut changes = Vec::new();
        let mut assets = Vec::new();

        match action {
            LibraryAction::CreateFolder { path } => {
                let relative = relative_path(&path)?;
                check_ancestors(&dir, relative)?;
                let mut db = index::initialize(&self.path)?;
                let tx = db.transaction()?;
                for folder in relative.ancestors().filter(|p| !p.as_os_str().is_empty()) {
                    tx.execute(
                        "INSERT OR IGNORE INTO folders(root,path) VALUES(?,?)",
                        params![config.root, folder.to_str()],
                    )?;
                }
                if let Some(parent) = relative.parent().filter(|p| !p.as_os_str().is_empty()) {
                    dir.create_dir_all(parent)?;
                }
                dir.create_dir(relative).map_err(|error| {
                    anyhow::anyhow!(tr!(
                        "knowledge.create_folder_failed",
                        path = path,
                        error = error
                    ))
                })?;
                // Disk is authoritative; the watcher reconciles even if the catalog commit fails
                let _ = self.wake.try_send(());
                tx.commit()?;
                self.state.lock().status.catalog_revision = Uuid::new_v4().to_string();
                return Ok(KnowledgeResult::Managed { paths: vec![path] });
            }
            LibraryAction::Create {
                path,
                content,
                format,
                metadata,
            } => {
                valid_path(&path)?;
                let xml = Path::new(&path)
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case("xml"));
                if let Some(format) = format {
                    ensure!(
                        (xml && format == "richtext") || (!xml && format == "markdown"),
                        "Format and extension must agree"
                    );
                }
                let body = if let Some(meta) = metadata {
                    ensure!(!xml, "Markdown front matter requires a Markdown document");
                    markdown::metadata(
                        &content,
                        meta.as_object()
                            .context("Expected metadata fields")?
                            .clone(),
                    )?
                } else {
                    content
                };
                changes.push(Change::create(path, body));
            }
            LibraryAction::Update {
                path,
                content,
                mode,
                revision,
            } => {
                let old = read(&path)?;
                ensure!(
                    revision
                        .as_ref()
                        .is_none_or(|r| r == &index::revision(&old)),
                    "The note changed since it was opened"
                );
                let new = match mode {
                    UpdateMode::Replace => content,
                    UpdateMode::Append => format!("{old}{content}"),
                    UpdateMode::Prepend => format!("{content}{old}"),
                };
                changes.push(Change::edit(path, old, new));
            }
            LibraryAction::Delete { path } => {
                let old = read(&path)?;
                changes.push(Change {
                    path,
                    before: Some(old),
                    after: None,
                });
            }
            LibraryAction::BatchTag {
                paths,
                tags,
                operation,
            } => {
                ensure!(
                    !tags.is_empty() && !paths.is_empty(),
                    "Select documents and tags"
                );
                let mut seen = HashSet::new();
                for path in paths {
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    ensure!(
                        index::is_markdown(valid_path(&path)?),
                        "Front matter tags require Markdown"
                    );
                    let old = read(&path)?;
                    let after = markdown::tag(&old, &tags, &operation)?;
                    changes.push(Change::edit(path, old, after));
                }
            }
            LibraryAction::Move { from, to } => {
                return self.place_entry(&config.root, &dir, &from, &to, None);
            }
            LibraryAction::Reorder {
                path,
                sibling,
                after,
            } => {
                let anchor = relative_path(&sibling)?;
                let name = relative_path(&path)?.file_name().unwrap();
                let to = anchor.with_file_name(name);
                return self.place_entry(
                    &config.root,
                    &dir,
                    &path,
                    &to.to_str().unwrap().replace(std::path::MAIN_SEPARATOR, "/"),
                    Some((&sibling, after)),
                );
            }
            LibraryAction::DeleteFolder { path } => {
                return self.trash_folder(&config.root, &dir, &path);
            }
            LibraryAction::ImportUrl { path, .. } => {
                ensure!(
                    index::is_markdown(valid_path(&path)?),
                    "Imports require a Markdown destination"
                );
                let article = article.context("Missing article")?;
                let parent = Path::new(&path).parent().unwrap();
                assets = article
                    .images
                    .into_iter()
                    .map(|(name, bytes)| (parent.join(name), bytes))
                    .collect();
                changes.push(Change::create(path, article.text));
            }
            LibraryAction::Restore { path, version } => {
                let before = current(&dir, valid_path(&path)?)?;
                let text = self
                    .history_db()?
                    .query_row(
                        "SELECT body FROM versions WHERE root=? AND path=? AND version=?",
                        params![config.root, path, version],
                        |r| r.get(0),
                    )
                    .context("Version not found")?;
                changes.push(Change {
                    path,
                    before,
                    after: Some(text),
                });
            }
        }
        // Validate the complete batch before the first write, then keep recoverable copies of previous text
        let reported = changes.iter().map(|c| c.path.clone()).collect();
        changes.retain(|c| c.before != c.after);
        let mut history = self.history_db()?;
        let tx = history.transaction()?;
        for c in &changes {
            ensure!(
                current(&dir, valid_path(&c.path)?)? == c.before,
                "Document changed or destination already exists: {}",
                c.path
            );
            if let Some(body) = &c.after {
                validate_body(Path::new(&c.path), body)?;
            }
            if let Some(old) = &c.before {
                save_version(&tx, &config.root, &c.path, old)?;
            }
        }
        tx.commit()?;
        let mut applied = 0;
        let mut assets_written = 0;
        let result = (|| -> Result<()> {
            for (path, bytes) in &assets {
                atomic_write(&dir, path, bytes, false)?;
                assets_written += 1;
            }
            for c in &changes {
                let path = Path::new(&c.path);
                ensure!(
                    current(&dir, path)? == c.before,
                    "Document was modified externally: {}",
                    c.path
                );
                match &c.after {
                    Some(body) => atomic_write(&dir, path, body.as_bytes(), c.before.is_some())?,
                    None => {
                        let trash = Path::new(".waku-trash")
                            .join(Uuid::new_v4().to_string())
                            .join(path);
                        dir.create_dir_all(trash.parent().unwrap())?;
                        dir.rename(path, &dir, trash)?;
                    }
                }
                applied += 1;
            }
            let mut db = index::initialize(&self.path)?;
            let tx = db.transaction()?;
            for c in &changes {
                if let Some(body) = &c.after {
                    index::prepare(&tx, &config.root, &c.path, &index::revision(body), "")?;
                } else {
                    tx.execute(
                        "DELETE FROM documents WHERE root=? AND path=?",
                        params![config.root, c.path],
                    )?;
                    tx.execute(
                        "DELETE FROM entry_order WHERE root=? AND path=?",
                        params![config.root, c.path],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })();
        if let Err(error) = result {
            for c in changes[..applied].iter().rev() {
                let path = Path::new(&c.path);
                if current(&dir, path)? == c.after {
                    match &c.before {
                        Some(old) => atomic_write(&dir, path, old.as_bytes(), c.after.is_some())?,
                        None => dir.remove_file(path)?,
                    }
                }
            }
            for (path, _) in &assets[..assets_written] {
                dir.remove_file(path)?;
            }
            return Err(error);
        }
        if !changes.is_empty() {
            let mut state = self.state.lock();
            state.generation += 1;
            state.status.indexing = true;
            let _ = self.wake.try_send(());
        }
        Ok(KnowledgeResult::Managed { paths: reported })
    }
}
