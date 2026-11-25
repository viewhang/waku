//! Personal Markdown library requests and daemon-owned source locations

use serde::{Deserialize, Serialize};
use std::path::Path;
use ts_rs::TS;
use uuid::Uuid;

/// Shared size limit for importing, reading and writing a library document
pub const MAX_NOTE_BYTES: u64 = 32 * 1024 * 1024;

/// Local imports and the index accept the same document formats
pub fn is_document(path: &Path) -> bool {
    is_markdown(path)
        || path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
}

/// Both Markdown extensions are case insensitive
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

/// A model selected from an existing Waku service
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeModel {
    pub service_id: Uuid,
    pub model: String,
}

/// The root names a directory on the daemon host
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LibraryConfig {
    pub root: String,
    pub embedding: KnowledgeModel,
}

/// Byte offsets refer to the UTF-8 text of exactly one document revision
#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeSource {
    pub document_id: Uuid,
    /// Display name of the saved library containing this document
    #[serde(default)]
    pub library: String,
    pub revision: String,
    pub path: String,
    pub heading: String,
    /// Cosine similarity before the lexical ranking boost
    pub similarity: f32,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStatus {
    pub config: Option<LibraryConfig>,
    /// Saved libraries in most recently activated order
    #[serde(default)]
    pub libraries: Vec<LibraryConfig>,
    pub indexing: bool,
    pub total: usize,
    pub ready: usize,
    pub catalog_revision: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
pub struct LibraryDocument {
    pub id: Uuid,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum KnowledgeOperation {
    Status,
    /// Apply the same note-management operation used by desktop and assistant tools
    Manage {
        action: LibraryAction,
    },
    /// Bind a delayed desktop tree action to its original library and document identity
    ManageTree {
        root: String,
        id: Option<Uuid>,
        action: LibraryAction,
    },
    /// A directory row keeps its immutable document identity across menu and dialog interactions
    DocumentFile {
        id: Uuid,
        action: DocumentFileAction,
    },
    History {
        path: String,
    },
    Source {
        path: String,
    },

    Configure {
        config: LibraryConfig,
    },
    /// Register folders together and activate the first selected folder
    AddLibraries {
        roots: Vec<String>,
        embedding: KnowledgeModel,
    },
    ActivateLibrary {
        root: String,
    },
    /// Open the saved library owning a cited document without trusting a caller-supplied path
    ActivateDocumentLibrary {
        id: Uuid,
    },
    /// Detach the directory from the library list while keeping the user's documents
    RemoveLibrary {
        root: String,
    },
    /// Re-embed the current library, including documents whose content is unchanged
    Rebuild,
    List {
        offset: usize,
        /// Include directory entries when paging the desktop tree
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        include_folders: Option<bool>,
    },
    Search {
        query: String,
    },
    Ask {
        query: String,
        chat: KnowledgeModel,
    },
    Read {
        id: Uuid,
        revision: Option<String>,
        start: usize,
        end: Option<usize>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DocumentFileAction {
    Source,
    Duplicate { to: String },
    Move { to: String },
    Delete,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum KnowledgeResult {
    Managed {
        paths: Vec<String>,
    },
    Versions {
        versions: Vec<LibraryVersion>,
    },
    Source {
        path: String,
        revision: String,
        text: String,
    },

    Status {
        status: LibraryStatus,
    },
    Documents {
        documents: Vec<LibraryDocument>,
        #[serde(default)]
        folders: Vec<String>,
        /// Only entries with a saved sibling position are included in this page
        #[serde(default)]
        positions: std::collections::HashMap<String, usize>,
        more: bool,
    },
    Matches {
        sources: Vec<KnowledgeSource>,
        #[serde(default)]
        warnings: Vec<String>,
    },
    Answer {
        text: String,
        sources: Vec<KnowledgeSource>,
        #[serde(default)]
        warnings: Vec<String>,
    },
    Passage {
        path: String,
        revision: String,
        changed: bool,
        text: String,
        next: Option<usize>,
    },
}

/// Note operations use relative library paths and optional optimistic revisions
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LibraryAction {
    CreateFolder {
        path: String,
    },
    Create {
        path: String,
        content: String,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
        #[serde(default)]
        format: Option<String>,
    },
    Update {
        path: String,
        content: String,
        #[serde(default)]
        mode: UpdateMode,
        #[serde(default)]
        revision: Option<String>,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    /// Place the entry immediately before or after a sibling, moving it into that parent if needed
    Reorder {
        path: String,
        sibling: String,
        #[serde(default)]
        after: bool,
    },
    /// Trash the entire directory, including files outside the document index
    DeleteFolder {
        path: String,
    },
    BatchTag {
        paths: Vec<String>,
        tags: Vec<String>,
        #[serde(default)]
        operation: TagMode,
    },
    ImportUrl {
        url: String,
        path: String,
        #[serde(default)]
        options: ImportOptions,
    },
    Restore {
        path: String,
        version: String,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    #[default]
    Replace,
    Append,
    Prepend,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum TagMode {
    #[default]
    Add,
    Remove,
}
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(default)]
pub struct ImportOptions {
    pub include_images: bool,
    pub clean_content: bool,
    pub add_metadata: bool,
}
impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            include_images: true,
            clean_content: true,
            add_metadata: true,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct LibraryVersion {
    pub version: String,
    pub saved_at: String,
}

/// Split a leading YAML header while preserving the byte offsets of the original Markdown
pub fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let (first, rest) = text.split_once('\n')?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if matches!(line.trim_end(), "---" | "...") {
            let header = &rest[..offset];
            return (header.trim().is_empty() || header.lines().any(|field| field.contains(':')))
                .then_some((header, &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Old drafts and chat citations can still contain this whole-note link
pub const LEGACY_NOTE_PREFIX: &str = "waku-knowledge://note/";

/// Resolve only Waku's document references, with byte offsets validated again on the daemon
pub fn citation_operation(target: &str) -> Option<KnowledgeOperation> {
    if let Some(id) = target.strip_prefix(LEGACY_NOTE_PREFIX) {
        return Some(KnowledgeOperation::Read {
            id: id.parse().ok()?,
            revision: None,
            start: 0,
            end: None,
        });
    }
    let (id, query) = target
        .strip_prefix("waku-knowledge://document/")?
        .split_once('?')?;
    let params: std::collections::HashMap<_, _> =
        query.split('&').filter_map(|s| s.split_once('=')).collect();
    let revision = *params.get("revision")?;
    if revision.len() != 64 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let start = params.get("start")?.parse().ok()?;
    let end = params.get("end")?.parse().ok()?;
    (end >= start).then_some(KnowledgeOperation::Read {
        id: id.parse().ok()?,
        revision: Some(revision.into()),
        start,
        end: Some(end),
    })
}
