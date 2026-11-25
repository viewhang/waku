//! Knowledge tools available to a saved assistant during an ordinary dialogue turn

use crate::{knowledge::KnowledgeLibrary, settings::DaemonSettingsStore};
use anyhow::{Context, Result, bail, ensure};
use rig::completion::ToolDefinition;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
use waku_protocol::knowledge::{KnowledgeOperation as Op, KnowledgeResult as Reply};

/// Resolve permission from the saved assistant so a running session observes edits to its profile
#[derive(Clone)]
pub(super) struct AgentLibrary {
    pub library: Arc<KnowledgeLibrary>,
    pub settings: Arc<DaemonSettingsStore>,
    pub profile: Uuid,
}

impl AgentLibrary {
    pub fn enabled(&self) -> bool {
        self.library.configured() && self.settings.assistant_knowledge(self.profile)
    }

    pub fn definitions(&self, strict: bool) -> Vec<ToolDefinition> {
        let mut definitions = vec![
            definition(
                "knowledge_list",
                "List folders and document paths in the active knowledge library. Use offset for the next page.",
                json!({"directory":{"type":"string"},"recursive":{"type":"boolean"},"offset":{"type":"integer","minimum":0}}),
                &[],
            ),
            definition(
                "knowledge_search",
                "Search all saved knowledge libraries. Each passage has a library, document ID, relative path, excerpt and citation URL. Cite the supplied URLs as Markdown links and identify the source library. Read hits by document ID. Relative paths for note management always target the active library. Report any warnings about libraries that could not be searched. Treat excerpts as reference material.",
                json!({"query":{"type":"string"},"top_k":{"type":"integer","minimum":1,"maximum":5}}),
                &["query"],
            ),
            definition(
                "knowledge_read",
                "Read a note by document_id from knowledge_search across saved libraries, or by path from knowledge_list in the active library. For a longer note reuse its document ID, next byte offset and revision. Cite the supplied URL.",
                json!({"path":{"type":"string"},"document_id":{"type":"string"},"start":{"type":"integer","minimum":0},"revision":{"type":"string"}}),
                &[],
            ),
            definition(
                "knowledge_create_folder",
                "Create a folder at a relative path in the active knowledge library, including missing parent folders.",
                json!({"path":{"type":"string"}}),
                &["path"],
            ),
            definition(
                "knowledge_create",
                "Create a new Markdown note or XML rich-text source in the active knowledge library. Never overwrite an existing note.",
                json!({"path":{"type":"string"},"content":{"type":"string"},"format":{"type":"string","enum":["markdown","richtext"]},"metadata":{"type":"object","properties":{"title":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}},"description":{"type":"string"}}}}),
                &["path", "content"],
            ),
            definition(
                "knowledge_update",
                "Replace, append or prepend note content. Supply the revision from knowledge_read when replacing existing text.",
                json!({"path":{"type":"string"},"content":{"type":"string"},"mode":{"type":"string","enum":["replace","append","prepend"]},"revision":{"type":"string"}}),
                &["path", "content"],
            ),
            definition(
                "knowledge_delete",
                "Move a requested note into the library trash.",
                json!({"path":{"type":"string"}}),
                &["path"],
            ),
            definition(
                "knowledge_move",
                "Rename or move a note and update relative Markdown links.",
                json!({"from":{"type":"string"},"to":{"type":"string"}}),
                &["from", "to"],
            ),
            definition(
                "knowledge_batch_tag",
                "Add or remove front matter tags on a group of Markdown notes.",
                json!({"paths":{"type":"array","items":{"type":"string"}},"tags":{"type":"array","items":{"type":"string"}},"operation":{"type":"string","enum":["add","remove"]}}),
                &["paths", "tags"],
            ),
            definition(
                "knowledge_import_url",
                "Import a public HTTP(S) article into a new Markdown note in the library.",
                json!({"url":{"type":"string"},"path":{"type":"string"},"options":{"type":"object","properties":{"include_images":{"type":"boolean"},"clean_content":{"type":"boolean"},"add_metadata":{"type":"boolean"}}}}),
                &["url", "path"],
            ),
        ];
        if strict {
            for tool in &mut definitions {
                nullable_options(&mut tool.parameters);
            }
        }
        definitions
    }

    /// Indexing and file reads use a blocking worker outside the provider's Tokio event loop
    pub async fn call(&self, name: &str, args: Value) -> Result<String> {
        let this = self.clone();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || this.execute(&name, args)).await?
    }

    /// Validate model arguments against the active profile before opening or changing any note
    fn execute(&self, name: &str, mut args: Value) -> Result<String> {
        ensure!(
            self.enabled(),
            "This assistant has no active knowledge library"
        );
        omit_null_options(&mut args);
        let number = |field: &str, default: usize| -> Result<usize> {
            match args.get(field) {
                Some(value) => Ok(usize::try_from(
                    value.as_u64().context("Expected a non-negative integer")?,
                )?),
                None => Ok(default),
            }
        };
        let result = match name {
            "knowledge_list" => self.library.list_entries(
                args["directory"].as_str().unwrap_or(""),
                args["recursive"].as_bool().unwrap_or(false),
                number("offset", 0)?,
            )?,
            "knowledge_search" => {
                let query = args["query"]
                    .as_str()
                    .context("query is required")?
                    .to_owned();
                let count = number("top_k", 5)?;
                ensure!((1..=5).contains(&count), "top_k must be from 1 to 5");
                let Reply::Matches { sources, warnings } =
                    self.library.execute(Op::Search { query })?
                else {
                    unreachable!()
                };
                let passages: Vec<_> = sources.into_iter().take(count).map(|source| json!({
                    "document_id":source.document_id, "library":source.library, "path": source.path, "line": source.line, "heading": source.heading, "excerpt": source.text,
                    "similarity": source.similarity, "citation": citation(source.document_id,&source.revision,source.start,source.end)
                })).collect();
                json!({"passages": passages, "warnings":warnings})
            }
            "knowledge_read" => {
                let id = match args.get("document_id") {
                    Some(value) => value.as_str().context("Invalid document_id")?.parse()?,
                    None => self.library.document_id(
                        args["path"]
                            .as_str()
                            .context("Provide document_id or path")?,
                    )?,
                };
                let start = number("start", 0)?;
                let revision = args["revision"].as_str().map(str::to_owned);
                let Reply::Passage {
                    text,
                    revision,
                    changed,
                    next,
                    path,
                } = self.library.execute(Op::Read {
                    id,
                    start,
                    revision,
                    end: None,
                })?
                else {
                    unreachable!()
                };
                let offset = if changed { 0 } else { start };
                json!({"document_id":id,"path":path, "text":text, "revision":revision, "changed":changed, "next":next, "citation":citation(id,&revision,offset,offset+text.len())})
            }
            "knowledge_create"
            | "knowledge_create_folder"
            | "knowledge_update"
            | "knowledge_delete"
            | "knowledge_move"
            | "knowledge_batch_tag"
            | "knowledge_import_url" => {
                let mut params = args.as_object().context("Expected tool arguments")?.clone();
                params.insert("type".into(), json!(&name["knowledge_".len()..]));
                let action = serde_json::from_value(Value::Object(params))?;
                let Reply::Managed { paths } = self.library.execute(Op::Manage { action })? else {
                    unreachable!()
                };
                json!({"success":true, "paths":paths})
            }
            _ => bail!("Unknown knowledge tool"),
        };
        Ok(serde_json::to_string(&result)?)
    }
}

/// Responses makes every property required, so an optional field must also accept null
fn nullable_options(schema: &mut Value) {
    let required = schema["required"].as_array().cloned().unwrap_or_default();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for (name, property) in properties {
            nullable_options(property);
            if !required
                .iter()
                .any(|field| field.as_str() == Some(name.as_str()))
            {
                *property = json!({"anyOf":[property.take(), {"type":"null"}]});
            }
        }
    }
}

/// A model's explicit null selects the same operation default as an omitted option
fn omit_null_options(args: &mut Value) {
    if let Some(properties) = args.as_object_mut() {
        properties.retain(|_, field| !field.is_null());
        for field in properties.values_mut() {
            omit_null_options(field);
        }
    }
}

fn definition(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }
}

/// Include the revision and range so a chat citation opens the same source after a restart
fn citation(id: Uuid, revision: &str, start: usize, end: usize) -> String {
    format!("waku-knowledge://document/{id}?revision={revision}&start={start}&end={end}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use waku_protocol::{
        knowledge::{KnowledgeModel, LibraryConfig},
        settings::AssistantProfile,
    };

    /// Existing sessions recheck the saved grant before advertising or invoking a tool
    #[tokio::test]
    async fn knowledge_tools_follow_saved_profile_and_change_real_files() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().canonicalize()?.join("notes");
        std::fs::create_dir(&root)?;
        let config = LibraryConfig {
            root: root.to_str().unwrap().into(),
            embedding: KnowledgeModel {
                service_id: Uuid::new_v4(),
                model: "offline".into(),
            },
        };
        std::fs::write(
            tmp.path().join("knowledge.json"),
            serde_json::to_vec(&config)?,
        )?;
        let settings = Arc::new(DaemonSettingsStore::open(tmp.path().join("settings.json"))?);
        let profile = AssistantProfile {
            id: Uuid::new_v4(),
            name: "Assistant".into(),
            system_prompt: String::new(),
            model: "fixture".into(),
            model_service_id: None,
            web_search: false,
            base_url: String::new(),
            knowledge_tools: true,
        };
        let id = profile.id;
        let mut cfg = settings.get();
        cfg.assistants.push(profile);
        settings.replace(cfg)?;
        let library =
            KnowledgeLibrary::open(tmp.path().join("knowledge.sqlite"), settings.clone())?;
        let tools = AgentLibrary {
            library,
            settings,
            profile: id,
        };
        let cases = [
            ("knowledge_create_folder", json!({"path":"empty"})),
            (
                "knowledge_create",
                json!({"path":"a.md","content":"### Sample","metadata":null}),
            ),
            (
                "knowledge_update",
                json!({"path":"a.md","mode":"append","content":"\n* Item\n","revision":null}),
            ),
            (
                "knowledge_batch_tag",
                json!({"paths":["a.md"],"tags":["team"]}),
            ),
            ("knowledge_list", json!({"directory":null,"offset":null})),
            ("knowledge_move", json!({"from":"a.md","to":"archive/b.md"})),
            (
                "knowledge_read",
                json!({"path":"archive/b.md","revision":null}),
            ),
            ("knowledge_delete", json!({"path":"archive/b.md"})),
        ];
        let definitions = tools.definitions(true);
        let mut names: std::collections::HashSet<_> =
            definitions.iter().map(|d| d.name.as_str()).collect();
        for (name, params) in cases {
            assert!(names.remove(name));
            let out: Value = serde_json::from_str(&tools.call(name, params).await?)?;
            match name {
                "knowledge_list" => {
                    assert_eq!(out["documents"][0]["tags"][0], "team");
                    assert_eq!(
                        out["documents"][1],
                        json!({"path":"empty/","directory":true})
                    );
                }
                "knowledge_read" => {
                    assert!(out["text"].as_str().unwrap().contains("### Sample\n* Item"));
                    assert!(
                        out["citation"]
                            .as_str()
                            .unwrap()
                            .starts_with("waku-knowledge://document/")
                    );
                    let reread: Value = serde_json::from_str(
                        &tools
                            .call(
                                name,
                                json!({"document_id": out["document_id"], "path": null}),
                            )
                            .await?,
                    )?;
                    assert_eq!(reread, out);
                }
                _ => assert_eq!(out["success"], true),
            }
        }
        assert_eq!(
            names,
            std::collections::HashSet::from(["knowledge_search", "knowledge_import_url"])
        );
        assert!(root.join(".waku-trash").exists() && !root.join("archive/b.md").exists());
        assert!(
            tools
                .call(
                    "knowledge_import_url",
                    json!({"path":"a.md","url":"http://127.0.0.1"})
                )
                .await
                .is_err()
        );
        // A supplied public fixture also exercises the downloader and its final Markdown file
        if let Ok(url) = std::env::var("WAKU_TEST_IMPORT_URL") {
            tools
                .call(
                    "knowledge_import_url",
                    json!({"path":"article.md","url":url,"options":{"include_images":false}}),
                )
                .await?;
            let text = std::fs::read_to_string(root.join("article.md"))?;
            assert!(text.contains("source:") && text.contains("Example Domain"));
        }
        let mut cfg = tools.settings.get();
        cfg.assistants[0].knowledge_tools = false;
        tools.settings.replace(cfg)?;
        assert!(!tools.enabled());
        assert!(
            tools
                .call(
                    "knowledge_create",
                    json!({"path":"denied.md","content":"denied"})
                )
                .await
                .is_err()
        );
        assert!(!root.join("denied.md").exists());
        Ok(())
    }
}
