//! Saved knowledge libraries on the daemon host share a single active index worker

mod catalog;
mod import;
mod index;
mod manage;
mod markdown;
mod retrieval;
#[cfg(test)]
mod tests;
mod tree;

use crate::{driver::assistant::knowledge::LibraryModel, settings::DaemonSettingsStore};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use notify::{RecursiveMode, Watcher};
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};
use waku_protocol::knowledge::*;

/// A configuration generation owns its progress and is checked before publishing work
#[derive(Default)]
struct State {
    generation: u64,
    status: LibraryStatus,
}

pub(crate) struct KnowledgeLibrary {
    path: PathBuf,
    settings: Arc<DaemonSettingsStore>,
    state: Mutex<State>,
    wake: Sender<()>,
    writes: Mutex<()>,
}

impl Drop for KnowledgeLibrary {
    fn drop(&mut self) {
        // The filesystem watcher also owns a sender while the worker is asleep
        let _ = self.wake.try_send(());
    }
}

impl KnowledgeLibrary {
    pub fn open(path: PathBuf, settings: Arc<DaemonSettingsStore>) -> Result<Arc<Self>> {
        let mut status = LibraryStatus::default();
        let config_path = path.with_extension("json");
        if config_path.exists() {
            match catalog::load(&config_path) {
                Ok(libraries) => {
                    status.config = libraries.first().cloned();
                    status.libraries = libraries;
                }
                Err(e) => status.error = Some(e.to_string()),
            }
        }
        let (wake, receiver) = bounded(1);
        let lib = Arc::new(Self {
            path,
            settings,
            state: Mutex::new(State {
                status,
                ..Default::default()
            }),
            wake,
            writes: Mutex::new(()),
        });
        let weak = Arc::downgrade(&lib);
        std::thread::Builder::new()
            .name("waku-knowledge-index".into())
            .spawn(move || watch(weak, receiver))?;
        Ok(lib)
    }

    pub fn execute(&self, op: KnowledgeOperation) -> Result<KnowledgeResult> {
        use KnowledgeOperation as Op;
        use KnowledgeResult as Reply;
        match op {
            Op::Status => Ok(Reply::Status {
                status: self.state.lock().status.clone(),
            }),
            Op::Configure { config } => self.save_roots(vec![config.root], config.embedding, true),
            Op::AddLibraries { roots, embedding } => self.save_roots(roots, embedding, false),
            Op::ActivateLibrary { root } => self.update_libraries(|libraries| {
                let index = catalog::position(libraries, &root)?;
                libraries[..=index].rotate_right(1);
                Ok(())
            }),
            Op::ActivateDocumentLibrary { id } => self.update_libraries(|libraries| {
                let db = index::connect(&self.path)?;
                let (root, _) = index::document_location(&db, id)?;
                let index = catalog::position(libraries, &root)?;
                libraries[..=index].rotate_right(1);
                Ok(())
            }),
            Op::RemoveLibrary { root } => self.update_libraries(|libraries| {
                let index = catalog::position(libraries, &root)?;
                libraries.remove(index);
                Ok(())
            }),
            Op::Rebuild => {
                let db = index::initialize(&self.path)?;
                let mut state = self.state.lock();
                let root = &state
                    .status
                    .config
                    .as_ref()
                    .context("Select a knowledge library first")?
                    .root;
                db.execute(
                    "UPDATE documents SET indexed_with=NULL WHERE root=?",
                    [root],
                )?;
                state.generation += 1;
                state.status.indexing = true;
                state.status.ready = 0;
                state.status.error = None;
                let _ = self.wake.try_send(());
                Ok(Reply::Status {
                    status: state.status.clone(),
                })
            }
            Op::Manage { action } => self.manage(action),
            Op::CreateUntitled {
                root,
                parent,
                stem,
                folder,
            } => self.create_untitled(&root, &parent, &stem, folder),
            Op::ManageTree { root, id, action } => self.manage_tree(&root, id, action),
            Op::DocumentFile { id, action } => self.document_file(id, action),
            op => {
                let (generation, config) = self.active()?;
                let db = index::initialize(&self.path)?;
                let result = match op {
                    Op::History { path } => Reply::Versions {
                        versions: self.versions(&config.root, &path)?,
                    },
                    Op::Source { path } => {
                        let text = manage::read_note(Path::new(&config.root), &path)?;
                        Reply::Source {
                            revision: index::revision(&text),
                            text,
                            path,
                        }
                    }
                    Op::List {
                        offset,
                        include_folders,
                    } => index::list(&db, &config.root, offset, include_folders.unwrap_or(false))?,
                    Op::Search { query } => {
                        let (sources, warnings) = self.retrieve(&db, generation, &query)?;
                        Reply::Matches { sources, warnings }
                    }
                    Op::Ask { query, chat } => {
                        let (sources, warnings) = self.retrieve(&db, generation, &query)?;
                        self.current(generation)?;
                        let (text, sources) = if sources.is_empty() {
                            (String::new(), Vec::new())
                        } else {
                            let evidence: Vec<_> = sources
                                .iter()
                                .enumerate()
                                .map(|(i, source)| {
                                    serde_json::json!({
                                        "number": i + 1, "library": source.library, "document": source.path,
                                        "heading": source.heading, "excerpt": source.text
                                    })
                                })
                                .collect();
                            let prompt = serde_json::json!({"question":query,"excerpts":evidence})
                                .to_string();
                            let answer = LibraryModel::resolve(&self.settings, &chat, false)?
                                .answer(prompt)?;
                            cited_answer(&answer, &sources)?
                        };
                        Reply::Answer {
                            text,
                            sources,
                            warnings,
                        }
                    }
                    Op::Read {
                        id,
                        revision,
                        start,
                        end,
                    } => {
                        let (root, path) = self.saved_document(&db, id)?;
                        let content = index::read(Path::new(&root), &path)?;
                        let latest = index::revision(&content);
                        let changed = revision.as_ref().is_some_and(|r| r != &latest);
                        let start = if changed { 0 } else { start };
                        let limit = if changed { None } else { end };
                        ensure!(
                            start <= content.len() && content.is_char_boundary(start),
                            "Invalid passage position"
                        );
                        let requested = limit.unwrap_or(content.len());
                        ensure!(
                            requested >= start
                                && requested <= content.len()
                                && content.is_char_boundary(requested),
                            "Invalid passage boundary"
                        );
                        let mut finish = requested.min(start.saturating_add(16_384));
                        while !content.is_char_boundary(finish) {
                            finish -= 1;
                        }
                        Reply::Passage {
                            path,
                            revision: latest,
                            changed,
                            text: content[start..finish].to_owned(),
                            next: (finish < content.len()).then_some(finish),
                        }
                    }
                    Op::Configure { .. }
                    | Op::AddLibraries { .. }
                    | Op::ActivateLibrary { .. }
                    | Op::ActivateDocumentLibrary { .. }
                    | Op::RemoveLibrary { .. }
                    | Op::ManageTree { .. }
                    | Op::CreateUntitled { .. }
                    | Op::Manage { .. }
                    | Op::DocumentFile { .. }
                    | Op::Status
                    | Op::Rebuild => {
                        unreachable!()
                    }
                };
                self.current(generation)?;
                Ok(result)
            }
        }
    }

    /// Recheck model availability and endpoint identity before issuing another batch
    pub(crate) fn service_changed(&self, id: uuid::Uuid) {
        let mut state = self.state.lock();
        if state
            .status
            .libraries
            .iter()
            .any(|c| c.embedding.service_id == id)
        {
            state.generation += 1;
            let _ = self.wake.try_send(());
        }
    }

    /// List immediate folders and files or all descendants without exposing host paths
    pub(crate) fn list_entries(
        &self,
        directory: &str,
        recursive: bool,
        offset: usize,
    ) -> Result<serde_json::Value> {
        let (generation, config) = self.active()?;
        let dir = directory.trim_end_matches('/');
        ensure!(Path::new(dir).components().all(|c| matches!(c,std::path::Component::Normal(s) if !s.to_string_lossy().starts_with('.'))) && !directory.starts_with('/') && !dir.contains('\\'),"Expected a library-relative folder");
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let db = index::initialize(&self.path)?;
        let mut stmt=db.prepare("WITH paths(path) AS (
            SELECT replace(path,char(92),'/') FROM documents WHERE root=?1
            UNION ALL SELECT path || '/' FROM folders WHERE root=?1
        ), entries AS (SELECT substr(path,length(?2)+1) AS tail FROM paths WHERE substr(path,1,length(?2))=?2 AND path!=?2)
        ,names AS (SELECT DISTINCT CASE WHEN ?3 OR instr(tail,'/')=0 THEN tail ELSE substr(tail,1,instr(tail,'/')) END name FROM entries)
        SELECT name FROM names LEFT JOIN entry_order o ON o.root=?1 AND o.path=?2 || rtrim(name,'/')
        ORDER BY CASE WHEN NOT ?3 THEN o.position END NULLS LAST, CASE WHEN NOT ?3 AND substr(name,-1)='/' THEN 0 ELSE 1 END, name LIMIT 51 OFFSET ?4")?;
        let mut paths = stmt
            .query_map(params![config.root, prefix, recursive, offset], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = paths.len() > 50;
        paths.truncate(50);
        let root = manage::root_dir(Path::new(&config.root))?;
        let next = has_more.then(|| offset + paths.len());
        let mut documents = Vec::with_capacity(paths.len());
        for entry in paths {
            let path = format!("{prefix}{entry}");
            if entry.ends_with('/') {
                documents.push(serde_json::json!({"path":path,"directory":true}));
            } else if let Some(details) = manage::entry_details(&root, &path)? {
                documents.push(details);
            }
        }
        self.current(generation)?;
        Ok(serde_json::json!({"documents":documents,"next":next}))
    }

    /// Old indexes may remain on disk after removal, so IDs must resolve inside the saved catalog
    fn saved_document(&self, db: &Connection, id: uuid::Uuid) -> Result<(String, String)> {
        let location = index::document_location(db, id)?;
        catalog::position(&self.state.lock().status.libraries, &location.0)?;
        Ok(location)
    }

    /// Resolve a library-relative tool argument through the catalog before opening the file
    pub(crate) fn document_id(&self, path: &str) -> Result<uuid::Uuid> {
        let (_, config) = self.active()?;
        let db = index::connect(&self.path)?;
        let id: String = db
            .query_row(
                "SELECT id FROM documents WHERE root=? AND path=?",
                [&config.root, path],
                |row| row.get(0),
            )
            .context("Document not found in the selected library")?;
        Ok(id.parse()?)
    }

    pub(crate) fn configured(&self) -> bool {
        self.state.lock().status.config.is_some()
    }

    fn active(&self) -> Result<(u64, LibraryConfig)> {
        let state = self.state.lock();
        Ok((
            state.generation,
            state
                .status
                .config
                .clone()
                .context("Select a knowledge library first")?,
        ))
    }

    fn current(&self, generation: u64) -> Result<()> {
        ensure!(
            self.state.lock().generation == generation,
            "The selected library changed"
        );
        Ok(())
    }

    fn progress(&self, generation: u64, root: &str, change: impl FnOnce(&mut LibraryStatus)) {
        let mut state = self.state.lock();
        if state.generation == generation
            && state.status.config.as_ref().is_some_and(|c| c.root == root)
        {
            change(&mut state.status);
        }
    }

    fn scan(
        &self,
        generation: u64,
        config: &LibraryConfig,
        revisions: &mut HashMap<String, String>,
        last_failure: &mut Option<String>,
    ) -> Result<()> {
        let catalog_write = self.writes.lock();
        let mut db = index::initialize(&self.path)?;
        let root = Path::new(&config.root);
        let index::Inventory { files, folders } = index::inventory(root)?;
        self.current(generation)?;
        if index::sync_folders(&mut db, &config.root, &folders)? {
            // Folder discovery also refreshes the tree when the embedding model is unavailable
            self.progress(generation, &config.root, |s| {
                s.catalog_revision = uuid::Uuid::new_v4().to_string()
            });
        }
        let model = LibraryModel::resolve(&self.settings, &config.embedding, true)?;
        let key = index_key(config, model.url.as_str());
        self.current(generation)?;
        index::forget_missing(&mut db, &config.root, &files)?;
        revisions.retain(|path, _| files.binary_search(path).is_ok());
        let mut pending = Vec::new();
        let mut ready = 0;
        let mut errors = Vec::new();
        let mut manifest = String::new();
        // A filesystem scan must not restore an old catalog stamp after a manual reorder
        let mut order =
            db.prepare("SELECT path,position FROM entry_order WHERE root=? ORDER BY path")?;
        let mut ranked = order.query([&config.root])?;
        while let Some(row) = ranked.next()? {
            let path: String = row.get(0)?;
            let position: usize = row.get(1)?;
            manifest.push_str(&format!("P{}:{path}:{position};", path.len()));
        }
        drop(ranked);
        drop(order);
        for path in &folders {
            manifest.push_str(&format!("D{}:{path}", path.len()));
        }
        for path in &files {
            self.current(generation)?;
            let rev = match revisions.get(path) {
                Some(revision) => revision.clone(),
                None => match index::read(root, path) {
                    Ok(text) => {
                        let revision = index::revision(&text);
                        revisions.insert(path.clone(), revision.clone());
                        revision
                    }
                    Err(e) => {
                        errors.push(format!("{path}: {e}"));
                        continue;
                    }
                },
            };
            manifest.push_str(&format!("{}:{path}{rev}", path.len()));
            let (id, done) = index::prepare(&db, &config.root, path, &rev, &key)?;
            if done {
                ready += 1;
            } else {
                pending.push((path, id, rev));
            }
        }
        self.progress(generation, &config.root, |s| {
            // Checking a library with current vectors should stay silent in the UI
            s.indexing = !pending.is_empty();
            s.error = None;
            s.total = files.len();
            s.ready = ready;
            s.catalog_revision = index::revision(&manifest);
        });
        drop(catalog_write);
        // A bad document gets retried without permanently starving later notes during an outage
        if let Some(failed) = last_failure.take() {
            let next = pending.partition_point(|(path, _, _)| *path <= &failed);
            pending.rotate_left(next);
        }
        for (path, id, rev) in pending {
            self.current(generation)?;
            let result = (|| -> Result<bool> {
                let text = index::read(root, path)?;
                if index::revision(&text) != rev {
                    return Ok(false);
                }
                let passages = index::chunks(&text);
                index::begin_document(&db)?;
                let mut dimension = None;
                for batch in passages.chunks(16) {
                    self.current(generation)?;
                    let inputs: Vec<_> = batch
                        .iter()
                        .map(|p| format!("{path}\n{}\n{}", p.heading, p.body))
                        .collect();
                    let vectors =
                        model.embed(&inputs.iter().map(String::as_str).collect::<Vec<_>>())?;
                    let width = vectors[0].len();
                    ensure!(
                        *dimension.get_or_insert(width) == width,
                        "Embedding dimension changed during indexing"
                    );
                    index::stage(&mut db, batch, &vectors)?;
                }
                if index::revision(&index::read(root, path)?) != rev {
                    return Ok(false);
                }
                let state = self.state.lock();
                ensure!(
                    state.generation == generation,
                    "Library configuration changed"
                );
                index::publish(&mut db, &id, path, &rev, &key)?;
                Ok(true)
            })();
            match result {
                Ok(true) => self.progress(generation, &config.root, |s| s.ready += 1),
                Ok(false) => {
                    revisions.remove(path);
                    let _ = self.wake.try_send(());
                }
                Err(e) => {
                    errors.push(format!("{path}: {e}"));
                    *last_failure = Some(path.clone());
                    break;
                }
            }
        }
        if let Some(error) = errors.first() {
            bail!("{error}");
        }
        Ok(())
    }
}

/// A changed endpoint, model, or chunk format requires its own compatible vectors
fn index_key(config: &LibraryConfig, url: &str) -> String {
    format!(
        "markdown-v2\n{}\n{}\n{}",
        config.embedding.service_id, url, config.embedding.model
    )
}

/// Keep each saved folder's filesystem events and revision cache alive across UI switches
#[derive(Default)]
struct WatchedFolder {
    watcher: Option<notify::RecommendedWatcher>,
    dirty: Arc<Mutex<Option<HashSet<PathBuf>>>>,
    revisions: HashMap<String, String>,
    last_failure: Option<String>,
    next_scan: Option<std::time::Instant>,
}

impl WatchedFolder {
    /// Install before the first inventory so edits made during embedding schedule another pass
    fn start(&mut self, lib: &KnowledgeLibrary, directory: &str) -> Result<()> {
        let wake = lib.wake.clone();
        let events = self.dirty.clone();
        let root = PathBuf::from(directory);
        // SQLite closes and removes its sidecar files even after read-only searches
        let index_files = ["", "-shm", "-wal", "-journal"].map(|suffix| {
            let mut name = lib.path.clone().into_os_string();
            name.push(suffix);
            PathBuf::from(name)
        });
        let mut next = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let mut pending = events.lock();
            match event {
                Ok(event) if !event.need_rescan() => {
                    if matches!(event.kind, notify::EventKind::Access(_)) {
                        return;
                    }
                    let mut changed = false;
                    for path in event.paths {
                        let Ok(relative) = path.strip_prefix(&root) else {
                            continue;
                        };
                        // Directory removals and renames may invalidate a whole subtree
                        if index_files.contains(&path)
                            || relative
                                .components()
                                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
                            || !(index::is_document(&path)
                                || path.is_dir()
                                || (!path.exists()
                                    && (event.kind.is_remove()
                                        || matches!(
                                            event.kind,
                                            notify::EventKind::Modify(
                                                notify::event::ModifyKind::Name(_)
                                            )
                                        ))))
                        {
                            continue;
                        }
                        changed = true;
                        if let Some(paths) = pending.as_mut() {
                            paths.insert(relative.to_owned());
                            if paths.len() >= 4096 {
                                *pending = None;
                            }
                        }
                    }
                    if !changed {
                        return;
                    }
                }
                _ => *pending = None,
            }
            let _ = wake.try_send(());
        })?;
        next.watch(Path::new(directory), RecursiveMode::Recursive)?;
        self.watcher = Some(next);
        Ok(())
    }
}

/// Reconcile registered roots on one worker and only publish progress for the displayed library
fn watch(weak: Weak<KnowledgeLibrary>, receiver: Receiver<()>) {
    use std::time::Instant;
    let mut folders = HashMap::<String, WatchedFolder>::new();
    let mut previous_generation = None;
    loop {
        let Some(lib) = weak.upgrade() else { return };
        let (generation, libraries) = {
            let state = lib.state.lock();
            (state.generation, state.status.libraries.clone())
        };
        folders.retain(|root, _| libraries.iter().any(|c| c.root == *root));
        let changed = previous_generation.replace(generation) != Some(generation);
        for (index, config) in libraries.iter().enumerate() {
            if lib.current(generation).is_err() {
                break;
            }
            let folder = folders.entry(config.root.clone()).or_default();
            let due = folder.next_scan.is_none_or(|time| Instant::now() >= time);
            // Drain before scanning so invalidations arriving during the scan survive for the next pass
            let pending = folder.dirty.lock().replace(HashSet::new());
            if !due && !changed && pending.as_ref().is_some_and(HashSet::is_empty) {
                continue;
            }
            if due || (changed && index == 0) {
                folder.revisions.clear();
            } else if let Some(paths) = pending {
                folder
                    .revisions
                    .retain(|path, _| !Path::new(path).ancestors().any(|p| paths.contains(p)));
            } else {
                folder.revisions.clear();
            }
            let result = (|| {
                if folder.watcher.is_none() {
                    folder.start(&lib, &config.root)?;
                }
                lib.scan(
                    generation,
                    config,
                    &mut folder.revisions,
                    &mut folder.last_failure,
                )
            })();
            let interval = if result.is_err() { 60 } else { 600 };
            folder.next_scan = Some(Instant::now() + Duration::from_secs(interval));
            lib.progress(generation, &config.root, |s| {
                s.indexing = false;
                s.error = result.err().map(|e| e.to_string());
            });
        }
        drop(lib);
        let delay = folders
            .values()
            .filter_map(|f| f.next_scan)
            .min()
            .map_or(Duration::from_secs(600), |time| {
                time.saturating_duration_since(Instant::now())
            });
        match receiver.recv_timeout(delay) {
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(180));
                while receiver.try_recv().is_ok() {}
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Only the server assigns citation labels, using IDs present in this retrieval
fn cited_answer(raw: &str, sources: &[KnowledgeSource]) -> Result<(String, Vec<KnowledgeSource>)> {
    #[derive(Deserialize)]
    struct Answer {
        claims: Vec<Claim>,
    }
    #[derive(Deserialize)]
    struct Claim {
        text: String,
        sources: Vec<usize>,
    }
    let json = raw.trim();
    let json = json
        .strip_prefix("```json")
        .or_else(|| json.strip_prefix("```"))
        .and_then(|v| v.strip_suffix("```"))
        .unwrap_or(json);
    let answer: Answer =
        serde_json::from_str(json.trim()).context("The model returned an invalid cited answer")?;
    let mut used = Vec::new();
    let mut lines = Vec::new();
    for claim in answer.claims {
        ensure!(
            !claim.text.trim().is_empty() && !claim.sources.is_empty(),
            "A claim has no supporting source"
        );
        let mut line = claim.text.trim().to_owned();
        for id in claim.sources {
            ensure!(
                id > 0 && id <= sources.len(),
                "Answer cites an unknown source"
            );
            let pos = match used.iter().position(|s| *s == id) {
                Some(i) => i,
                None => {
                    used.push(id);
                    used.len() - 1
                }
            };
            line.push_str(&format!(" [{}]", pos + 1));
        }
        lines.push(line);
    }
    Ok((
        lines.join("\n\n"),
        used.into_iter().map(|i| sources[i - 1].clone()).collect(),
    ))
}
