//! Tree mutations keep document identities, link targets and sibling positions together

use super::{KnowledgeLibrary, index, manage, markdown};
use anyhow::{Context, Result, ensure};
use cap_std::fs::Dir;
use rusqlite::{Connection, params};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;
use waku_protocol::knowledge::KnowledgeResult;

/// Match whole path components even when a filename contains SQL wildcard characters
const SUBTREE: &str = "(path=?2 OR substr(path,1,length(?2)+1)=?2||'/')";

/// Map an entire subtree without appending a trailing separator to the moved entry itself
pub(super) fn relocated_path(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let tail = path.strip_prefix(from).ok()?;
    let mut result = to.to_owned();
    result.extend(tail.components());
    Some(result)
}

/// Anchor the rename in its parent directories and atomically reject an occupied destination
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn rename_new(dir: &Dir, _root: &Path, from: &Path, to: &Path) -> Result<()> {
    let parent = |path: &Path| match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => dir.open_dir(p),
        None => dir.try_clone(),
    };
    rustix::fs::renameat_with(
        parent(from)?,
        from.file_name().unwrap(),
        parent(to)?,
        to.file_name().unwrap(),
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    Ok(())
}

/// MoveFileW refuses to replace an existing file or directory
#[cfg(windows)]
fn rename_new(dir: &Dir, root: &Path, from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    // Windows directory handles deny delete sharing and pin both parent chains until the move finishes
    let mut parents = Vec::new();
    for path in [from, to] {
        for ancestor in path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            parents.push(dir.open_dir(ancestor)?);
        }
        manage::check_ancestors(dir, path)?;
    }
    let wide = |p: &Path| {
        root.join(p)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let (source, target) = (wide(from), wide(to));
    // Both buffers are NUL-terminated and live through the native call
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileW(source.as_ptr(), target.as_ptr())
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Load only direct siblings and reproduce the default folder-first order for unranked entries
fn save_position(
    db: &Connection,
    dir: &Dir,
    root: &str,
    path: &str,
    sibling: &str,
    after: bool,
) -> Result<()> {
    let parent = Path::new(sibling).parent().unwrap();
    let folder = if parent.as_os_str().is_empty() {
        dir.try_clone()?
    } else {
        dir.open_dir(parent)?
    };
    let prefix = sibling.rfind('/').map_or("", |i| &sibling[..=i]);
    let mut stmt = db.prepare(
        "SELECT path,position FROM entry_order
         WHERE root=?1 AND substr(path,1,length(?2))=?2 AND instr(substr(path,length(?2)+1),'/')=0",
    )?;
    let ranks: HashMap<String, usize> = stmt
        .query_map([root, prefix], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut entries = Vec::new();
    for entry in folder.entries()? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        let relative = parent.join(&name);
        if name.to_string_lossy().starts_with('.')
            || !(ty.is_dir() || ty.is_file() && index::is_document(&relative))
        {
            continue;
        }
        let key = relative
            .to_str()
            .context("Invalid directory entry")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if key != path {
            entries.push((
                ranks.get(&key).copied().unwrap_or(usize::MAX),
                !ty.is_dir(),
                key,
            ));
        }
    }
    entries.sort();
    let mut entries: Vec<_> = entries.into_iter().map(|(_, _, p)| p).collect();
    let index = entries
        .iter()
        .position(|p| p == sibling)
        .context("The target sibling no longer exists")?;
    entries.insert(index + usize::from(after), path.into());
    for (position, entry) in entries.iter().enumerate() {
        db.execute("INSERT INTO entry_order VALUES(?1,?2,?3) ON CONFLICT(root,path) DO UPDATE SET position=excluded.position",params![root,entry,position])?;
    }
    Ok(())
}

impl KnowledgeLibrary {
    /// Pure reorders update only the catalog and never schedule embedding work
    pub(super) fn place_entry(
        &self,
        root: &str,
        dir: &Dir,
        from: &str,
        to: &str,
        anchor: Option<(&str, bool)>,
    ) -> Result<KnowledgeResult> {
        let src = manage::relative_path(from)?;
        let dst = manage::relative_path(to)?;
        manage::check_ancestors(dir, src)?;
        manage::check_ancestors(dir, dst)?;
        let meta = dir.symlink_metadata(src)?;
        ensure!(
            meta.is_dir() || meta.is_file() && index::is_document(src) && index::is_document(dst),
            "Expected a library folder or document"
        );
        ensure!(
            from == to || !dst.starts_with(src),
            "Cannot move a folder into itself or a descendant"
        );
        let mut db = index::initialize(&self.path)?;
        if let Some((sibling, _)) = anchor {
            ensure!(from != sibling && to != sibling, "Choose another sibling");
            manage::check_ancestors(dir, Path::new(sibling))?;
            let entry = dir
                .symlink_metadata(sibling)
                .context("The target sibling no longer exists")?;
            ensure!(
                entry.is_dir() || entry.is_file() && index::is_document(Path::new(sibling)),
                "Expected a folder or document as the target sibling"
            );
        }
        if from == to {
            if let Some((sibling, after)) = anchor {
                let tx = db.transaction()?;
                save_position(&tx, dir, root, from, sibling, after)?;
                tx.commit()?;
                let mut state = self.state.lock();
                if state.status.config.as_ref().is_some_and(|c| c.root == root) {
                    state.status.catalog_revision = Uuid::new_v4().to_string();
                }
            }
            return Ok(KnowledgeResult::Managed {
                paths: vec![to.into()],
            });
        }
        ensure!(!dir.try_exists(dst)?, "Destination already exists: {to}");
        let root_path = Path::new(root);
        let mut edits = Vec::new();
        // Resolve links against their old owner and write only documents whose text actually changes
        self.history_db()?;
        db.execute(
            "ATTACH DATABASE ? AS history",
            [self
                .path
                .with_file_name("knowledge-history.sqlite")
                .to_str()
                .context("Invalid history path")?],
        )?;
        let tx = db.transaction()?;
        for table in ["documents", "folders", "entry_order", "history.versions"] {
            tx.execute(&format!("UPDATE {table} SET path=?3||substr(path,length(?2)+1) WHERE root=?1 AND {SUBTREE}"),params![root,from,to])?;
        }
        // Embedding input includes the path even when a moved document keeps its original bytes
        tx.execute(
            &format!("UPDATE documents SET indexed_with=NULL WHERE root=?1 AND {SUBTREE}"),
            [root, to],
        )?;
        if src.parent() != dst.parent() {
            tx.execute(
                "DELETE FROM entry_order WHERE root=? AND path=?",
                [root, to],
            )?;
        }
        for path in index::inventory(root_path)?.files {
            let moved = relocated_path(Path::new(&path), src, dst);
            if !index::is_markdown(Path::new(&path)) && moved.is_none() {
                continue;
            }
            let original = manage::read_note(root_path, &path)?;
            let body = if index::is_markdown(Path::new(&path)) {
                markdown::moved_links(&original, root_path, &path, from, to)?
            } else {
                original.clone()
            };
            let relocated = moved.is_some();
            let new_path = moved.unwrap_or_else(|| PathBuf::from(&path));
            let key = new_path
                .to_str()
                .unwrap()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if body != original || path == from {
                manage::save_version(&tx, root, &key, &original)?;
            }
            if path == from && !index::is_markdown(dst) {
                manage::validate_body(dst, &body)?;
            }
            if relocated || body != original {
                index::prepare(&tx, root, &key, &index::revision(&body), "")?;
            }
            if body != original {
                edits.push((new_path, original, body));
            }
        }
        if let Some(parent) = dst.parent().filter(|p| !p.as_os_str().is_empty()) {
            dir.create_dir_all(parent)?;
            for folder in parent.ancestors().filter(|p| !p.as_os_str().is_empty()) {
                tx.execute(
                    "INSERT OR IGNORE INTO folders VALUES(?,?)",
                    params![
                        root,
                        folder
                            .to_str()
                            .unwrap()
                            .replace(std::path::MAIN_SEPARATOR, "/")
                    ],
                )?;
            }
        }
        if meta.is_dir() {
            tx.execute("INSERT OR IGNORE INTO folders VALUES(?,?)", [root, to])?;
        }
        if let Some((sibling, after)) = anchor {
            save_position(&tx, dir, root, to, sibling, after)?;
        }
        rename_new(dir, root_path, src, dst)?;
        let mut applied = 0;
        let result = (|| -> Result<()> {
            for (path, old, new) in &edits {
                ensure!(
                    dir.read_to_string(path)? == *old,
                    "Document changed externally: {}",
                    path.display()
                );
                manage::atomic_write(dir, path, new.as_bytes(), true)?;
                applied += 1;
            }
            tx.commit()?;
            Ok(())
        })();
        if let Err(error) = result {
            for (path, old, new) in edits[..applied].iter().rev() {
                if dir.read_to_string(path)? == *new {
                    manage::atomic_write(dir, path, old.as_bytes(), true)?;
                }
            }
            rename_new(dir, root_path, dst, src)?;
            return Err(error);
        }
        self.tree_changed(root);
        Ok(KnowledgeResult::Managed {
            paths: vec![to.into()],
        })
    }

    /// Move the complete subtree to one recoverable location and remove its catalog entries together
    pub(super) fn trash_folder(
        &self,
        root: &str,
        dir: &Dir,
        path: &str,
    ) -> Result<KnowledgeResult> {
        let source = manage::relative_path(path)?;
        manage::check_ancestors(dir, source)?;
        ensure!(dir.symlink_metadata(source)?.is_dir(), "Expected a folder");
        let trash = Path::new(".waku-trash")
            .join(Uuid::new_v4().to_string())
            .join(source);
        manage::check_ancestors(dir, &trash)?;
        dir.create_dir_all(trash.parent().unwrap())?;
        let mut db = index::initialize(&self.path)?;
        let tx = db.transaction()?;
        for table in ["documents", "folders", "entry_order"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE root=?1 AND {SUBTREE}"),
                params![root, path],
            )?;
        }
        rename_new(dir, Path::new(root), source, &trash)?;
        if let Err(error) = tx.commit() {
            rename_new(dir, Path::new(root), &trash, source)?;
            return Err(error.into());
        }
        self.tree_changed(root);
        Ok(KnowledgeResult::Managed {
            paths: vec![path.into()],
        })
    }

    /// Publish path changes immediately and invalidate workers still using the previous paths
    fn tree_changed(&self, root: &str) {
        let mut state = self.state.lock();
        state.generation += 1;
        if state.status.config.as_ref().is_some_and(|c| c.root == root) {
            state.status.catalog_revision = Uuid::new_v4().to_string();
        }
        let _ = self.wake.try_send(());
    }
}
