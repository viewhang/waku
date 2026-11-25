//! SQLite owns document identity and publishes each complete set of passages in one transaction

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use waku_protocol::knowledge::{KnowledgeResult, KnowledgeSource, LibraryDocument};

pub(super) use waku_protocol::knowledge::{MAX_NOTE_BYTES, is_document, is_markdown};
const CHUNK_CHARS: usize = 1200;

pub(super) fn connect(path: &Path) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA foreign_keys=ON")?;
    Ok(db)
}

/// Only derived passages and index metadata live here, the Markdown files are authoritative
pub(super) fn initialize(path: &Path) -> Result<Connection> {
    let db = connect(path)?;
    db.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS documents(
            id TEXT PRIMARY KEY, root TEXT NOT NULL, path TEXT NOT NULL, revision TEXT NOT NULL,
            indexed_with TEXT, UNIQUE(root,path));
        CREATE TABLE IF NOT EXISTS folders(root TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(root,path));
        CREATE TABLE IF NOT EXISTS passages(
            doc TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
            start INTEGER NOT NULL, end INTEGER NOT NULL, line INTEGER NOT NULL,
            heading TEXT NOT NULL, body TEXT NOT NULL, vector BLOB NOT NULL, PRIMARY KEY(doc,start));")?;
    Ok(db)
}

/// SHA-256 identifies the exact UTF-8 revision used for citation offsets
pub(super) fn revision(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Indexing, citations and editing share the same directory-scoped reader
pub(super) use super::manage::read_note as read;

/// Only files enter the embedding pipeline, while directories remain visible even when empty
pub(super) struct Inventory {
    pub files: Vec<String>,
    pub folders: Vec<String>,
}

/// Collect all visible directory paths and supported documents in a single traversal
pub(super) fn inventory(root: &Path) -> Result<Inventory> {
    ensure!(root.canonicalize()? == root, "Library root has moved");
    let mut dirs = vec![PathBuf::from(root)];
    let mut files = Vec::new();
    let mut folders = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let kind = entry.file_type()?;
            let path = entry.path();
            if !(kind.is_dir() || kind.is_file() && is_document(&path)) {
                continue;
            }
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("Non-UTF-8 library path")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if kind.is_dir() {
                folders.push(relative);
                dirs.push(path);
            } else {
                files.push(relative);
            }
        }
    }
    files.sort();
    folders.sort();
    Ok(Inventory { files, folders })
}

/// Reconcile new, renamed and removed folders without rewriting an unchanged catalog
pub(super) fn sync_folders(db: &mut Connection, root: &str, folders: &[String]) -> Result<bool> {
    let mut stored = db
        .prepare("SELECT path FROM folders WHERE root=?")?
        .query_map([root], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    let tx = db.transaction()?;
    let mut changed = false;
    for path in folders {
        if !stored.remove(path) {
            tx.execute("INSERT INTO folders(root,path) VALUES(?,?)", [root, path])?;
            changed = true;
        }
    }
    for path in stored {
        tx.execute("DELETE FROM folders WHERE root=? AND path=?", [root, &path])?;
        changed = true;
    }
    tx.commit()?;
    Ok(changed)
}

/// Catalog IDs are unique across roots, including identically named notes
pub(super) fn document_location(db: &Connection, id: Uuid) -> Result<(String, String)> {
    db.query_row(
        "SELECT root,path FROM documents WHERE id=?",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .context("This document is no longer in the library")
}

/// Page both kinds of entry together while preserving document-only requests from older clients
pub(super) fn list(
    db: &Connection,
    root: &str,
    offset: usize,
    include_folders: bool,
) -> Result<KnowledgeResult> {
    let mut stmt = db.prepare("SELECT id,path FROM documents WHERE root=?1
        UNION ALL SELECT NULL,path FROM folders WHERE root=?1 AND ?2 ORDER BY path LIMIT 51 OFFSET ?3")?;
    let mut rows = stmt.query(params![root, include_folders, offset])?;
    let mut documents = Vec::new();
    let mut folders = Vec::new();
    let mut more = false;
    while let Some(row) = rows.next()? {
        if documents.len() + folders.len() == 50 {
            more = true;
            break;
        }
        let path = row.get(1)?;
        match row.get::<_, Option<String>>(0)? {
            Some(id) => documents.push(LibraryDocument {
                id: id.parse()?,
                path,
            }),
            None => folders.push(path),
        }
    }
    Ok(KnowledgeResult::Documents {
        documents,
        folders,
        more,
    })
}

pub(super) struct Passage<'a> {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub heading: String,
    pub body: &'a str,
}

/// Preserve paragraph boundaries, heading ancestry and UTF-8 offsets including in long CJK lines
pub(super) fn chunks(text: &str) -> Vec<Passage<'_>> {
    let mut out = Vec::new();
    let mut heads: Vec<(usize, &str)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut start = 0;
    let mut offset = 0;
    let mut line = 1;
    let mut start_line = 1;
    let mut count = 0;
    let mut paragraph_end = None;
    for raw in text.split_inclusive('\n') {
        let s = raw.trim();
        let level = s.bytes().take_while(|b| *b == b'#').count();
        let heading = fence.is_none()
            && (1..=6).contains(&level)
            && s[level..].starts_with(char::is_whitespace);
        let blank = fence.is_none() && s.is_empty();
        let line_chars = raw.chars().count();
        // Pack nearby paragraphs under one heading and prefer a blank line at the size boundary
        if !heading
            && count + line_chars > CHUNK_CHARS
            && let Some((end, next_line)) = paragraph_end.take()
        {
            emit(text, start, end, start_line, &heads, &mut out);
            start = end;
            start_line = next_line;
            count = text[end..offset].chars().count();
        }
        if heading || count + line_chars > CHUNK_CHARS {
            emit(text, start, offset, start_line, &heads, &mut out);
            start = offset;
            start_line = line;
            count = 0;
            paragraph_end = None;
        }
        if heading {
            heads.retain(|(n, _)| *n < level);
            // Bound repeated heading context even if an entire unwrapped note starts with a hash
            let title = s[level..].trim();
            let end = title
                .char_indices()
                .nth(160)
                .map_or(title.len(), |(i, _)| i);
            heads.push((level, &title[..end]));
        }
        if let Some(marker @ ('`' | '~')) = s.chars().next() {
            let width = s.chars().take_while(|c| *c == marker).count();
            if width >= 3 {
                match fence {
                    Some((c, n)) if marker == c && width >= n && s[width..].is_empty() => {
                        fence = None
                    }
                    None => fence = Some((marker, width)),
                    _ => {}
                }
            }
        }
        for (i, _) in raw.char_indices() {
            if count == CHUNK_CHARS {
                emit(text, start, offset + i, start_line, &heads, &mut out);
                start = offset + i;
                start_line = line;
                count = 0;
            }
            count += 1;
        }
        offset += raw.len();
        line += 1;
        if blank {
            paragraph_end = Some((offset, line));
            if count == line_chars {
                start = offset;
                start_line = line;
                count = 0;
                paragraph_end = None;
            }
        }
    }
    emit(text, start, offset, start_line, &heads, &mut out);
    out
}

fn emit<'a>(
    text: &'a str,
    start: usize,
    end: usize,
    line: usize,
    heads: &[(usize, &str)],
    out: &mut Vec<Passage<'a>>,
) {
    let body = text[start..end].trim_end();
    if !body.trim().is_empty() {
        out.push(Passage {
            start,
            end: start + body.len(),
            line,
            heading: heads
                .iter()
                .map(|(_, h)| *h)
                .collect::<Vec<_>>()
                .join(" › "),
            body,
        });
    }
}

/// Announce pending revisions before asking an embedding service to process the file
pub(super) fn prepare(
    db: &Connection,
    root: &str,
    path: &str,
    rev: &str,
    key: &str,
) -> Result<(String, bool)> {
    let existing: Option<(String, String, Option<String>)> = db
        .query_row(
            "SELECT id,revision,indexed_with FROM documents WHERE root=? AND path=?",
            [root, path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((id, old, indexed)) = existing {
        if rev == old {
            return Ok((id, indexed.as_deref() == Some(key)));
        }
        db.execute(
            "UPDATE documents SET revision=?,indexed_with=NULL WHERE id=?",
            params![rev, id],
        )?;
        Ok((id, false))
    } else {
        let id = Uuid::new_v4().to_string();
        db.execute(
            "INSERT INTO documents(id,root,path,revision) VALUES(?,?,?,?)",
            params![id, root, path, rev],
        )?;
        Ok((id, false))
    }
}

pub(super) fn forget_missing(db: &mut Connection, root: &str, files: &[String]) -> Result<()> {
    let present: HashSet<_> = files.iter().map(String::as_str).collect();
    let tx = db.transaction()?;
    let paths = tx
        .prepare("SELECT path FROM documents WHERE root=?")?
        .query_map([root], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for path in paths {
        if !present.contains(path.as_str()) {
            tx.execute(
                "DELETE FROM documents WHERE root=? AND path=?",
                [root, &path],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// A connection-private staging table bounds vector memory to one service batch
pub(super) fn begin_document(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS staged (
        start INTEGER, end INTEGER, line INTEGER, heading TEXT, body TEXT, vector BLOB);
        DELETE FROM staged",
    )?;
    Ok(())
}

pub(super) fn stage(
    db: &mut Connection,
    chunks: &[Passage<'_>],
    vectors: &[Vec<f32>],
) -> Result<()> {
    ensure!(chunks.len() == vectors.len(), "Incomplete passage vectors");
    let tx = db.transaction()?;
    {
        let mut insert = tx.prepare_cached("INSERT INTO staged VALUES(?,?,?,?,?,?)")?;
        for (p, v) in chunks.iter().zip(vectors) {
            let bytes: Vec<u8> = v.iter().flat_map(|v| v.to_le_bytes()).collect();
            insert.execute(params![p.start, p.end, p.line, p.heading, p.body, bytes])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Readers see all passages from exactly one complete revision
pub(super) fn publish(db: &mut Connection, id: &str, rev: &str, key: &str) -> Result<()> {
    let tx = db.transaction()?;
    ensure!(
        tx.execute(
            "UPDATE documents SET indexed_with=? WHERE id=? AND revision=?",
            [key, id, rev]
        )? == 1,
        "Note revision changed"
    );
    tx.execute("DELETE FROM passages WHERE doc=?", [id])?;
    tx.execute(
        "INSERT INTO passages(doc,start,end,line,heading,body,vector)
                SELECT ?,start,end,line,heading,body,vector FROM staged",
        [id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Match Latin words and identifiers as units and unsegmented CJK text as adjacent character pairs
fn query_terms(query: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut word_start = None;
    let mut previous_cjk = None;
    for (i, c) in query
        .char_indices()
        .chain(std::iter::once((query.len(), ' ')))
    {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            word_start.get_or_insert(i);
        } else if let Some(start) = word_start.take() {
            let term = &query[start..i];
            if term.bytes().any(|b| b.is_ascii_alphanumeric()) {
                terms.push(term);
            }
        }
        if matches!(c as u32, 0x3400..=0x9fff | 0x3040..=0x30ff | 0xac00..=0xd7af | 0x20000..=0x323af)
        {
            if let Some(start) = previous_cjk.replace(i) {
                terms.push(&query[start..i + c.len_utf8()]);
            }
        } else {
            previous_cjk = None;
        }
        // Bound lexical work per passage while the embedding still sees the full question
        if terms.len() >= 64 {
            break;
        }
    }
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Keep a bounded top list while scanning normalized vectors outside the UI thread
pub(super) fn search(
    db: &Connection,
    root: &str,
    key: &str,
    query: &str,
    vector: &[f32],
) -> Result<Vec<(f32, KnowledgeSource)>> {
    let mut stmt = db.prepare(
        "SELECT id,path,revision,start,end,line,heading,body,vector
         FROM passages p JOIN documents d ON p.doc=d.id
         WHERE root=? AND indexed_with=? ORDER BY path,start",
    )?;
    let mut rows = stmt.query([root, key])?;
    let lower = query.to_lowercase();
    let terms = query_terms(&lower);
    let identifiers: Vec<_> = terms
        .iter()
        .copied()
        .filter(|term| term.bytes().any(|b| b.is_ascii_digit()))
        .collect();
    let mut best: Vec<(f32, KnowledgeSource)> = Vec::new();
    while let Some(r) = rows.next()? {
        let bytes = r.get_ref(8)?.as_blob()?;
        ensure!(
            bytes.len() == 4 * vector.len(),
            "Embedding dimension changed; rebuild the library index"
        );
        let cosine = bytes
            .chunks_exact(4)
            .zip(vector)
            .map(|(b, q)| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * q)
            .sum::<f32>();
        let path = r.get_ref(1)?.as_str()?;
        let heading = r.get_ref(6)?.as_str()?;
        let body = r.get_ref(7)?.as_str()?;
        let combined = format!("{path}\n{heading}\n{body}").to_lowercase();
        let keyword = terms.iter().filter(|t| combined.contains(**t)).count() as f32
            / terms.len().max(1) as f32;
        let identifier_match = identifiers.iter().any(|id| combined.contains(id));
        let score = cosine
            + 0.65 * keyword
            + if combined.contains(&lower) || identifier_match {
                0.5
            } else {
                0.0
            };
        let rank = best.partition_point(|(other, _)| *other >= score);
        if rank >= 8 {
            continue;
        }
        best.insert(
            rank,
            (
                score,
                KnowledgeSource {
                    document_id: r.get_ref(0)?.as_str()?.parse()?,
                    library: root.to_owned(),
                    path: path.into(),
                    revision: r.get(2)?,
                    start: r.get(3)?,
                    end: r.get(4)?,
                    line: r.get(5)?,
                    heading: heading.into(),
                    similarity: cosine.clamp(-1.0, 1.0),
                    text: body.into(),
                },
            ),
        );
        best.truncate(8);
    }
    Ok(best)
}
