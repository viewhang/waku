//! Exercise the desktop wire operations against an isolated daemon, SQLite index and model service

use super::*;
use crate::{
    ServerOptions, daemon::WakuBackend, driver::assistant::tests as http, persistence::StateStore,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
    time::Instant,
};
use uuid::Uuid;
use waku_client::DaemonClient;
use waku_protocol::{
    Command, ResponsePayload,
    settings::{ModelKind, ServiceModel},
};

/// Use the same websocket client and command dispatcher as the desktop page
fn start_daemon(dir: &Path) -> (DaemonClient, thread::JoinHandle<()>) {
    let backend = WakuBackend::new(
        DaemonSettingsStore::open(dir.join("settings.json")).unwrap(),
        StateStore::daemon(dir.join("app.db")),
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = thread::spawn(move || {
        crate::serve(
            listener,
            "fixture".into(),
            Arc::new(backend),
            Arc::new(AtomicBool::new(false)),
            ServerOptions {
                allow_shutdown: true,
                ..Default::default()
            },
        )
        .unwrap()
    });
    (
        DaemonClient::connect(&address, "fixture".into()).unwrap(),
        server,
    )
}

fn call(client: &DaemonClient, operation: KnowledgeOperation) -> KnowledgeResult {
    match client
        .request(Uuid::nil(), Uuid::nil(), Command::Knowledge { operation })
        .unwrap()
    {
        ResponsePayload::Knowledge { result } => result,
        other => panic!("unexpected response {other:?}"),
    }
}

/// Wait for the expected revision so a pre-event ready status cannot pass the test
fn ready(client: &DaemonClient, count: usize, previous: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let KnowledgeResult::Status { status } = call(client, KnowledgeOperation::Status) else {
            panic!()
        };
        assert!(status.error.is_none(), "{:?}", status.error);
        if !status.indexing
            && status.ready == count
            && !status.catalog_revision.is_empty()
            && status.catalog_revision != previous
        {
            return status.catalog_revision;
        }
        assert!(Instant::now() < deadline, "index timed out: {status:?}");
        thread::sleep(Duration::from_millis(40));
    }
}

/// Deterministic semantic axes isolate ranking and reversed batch order from model variability
fn embedding(text: &str) -> [f32; 4] {
    let release = ["暂缓", "延期"].iter().any(|word| text.contains(word));
    let owner = ["负责人", "谁跟进"].iter().any(|word| text.contains(word));
    let backup = text.contains("备份");
    [
        release as u8 as f32,
        owner as u8 as f32,
        backup as u8 as f32,
        0.01,
    ]
}

/// Read model input and select actual supplied evidence numbers for two distinct documents
fn claims(payload: &Value) -> Value {
    let content = payload["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    let prompt = content
        .as_str()
        .or_else(|| content[0]["text"].as_str())
        .unwrap();
    let input: Value = serde_json::from_str(prompt).unwrap();
    if input["question"].as_str().unwrap().contains("预算") {
        return json!({"claims":[]});
    }
    let evidence = input["excerpts"].as_array().unwrap();
    if input["question"] == "跨库核对决策和备份" {
        let ids: Vec<_> = [("notes", "复核完成"), ("work", "工作库专用")]
            .into_iter()
            .map(|(library, excerpt)| {
                &evidence
                    .iter()
                    .find(|e| {
                        e["library"] == library
                            && e["document"] == "decision.md"
                            && e["excerpt"].as_str().unwrap().contains(excerpt)
                    })
                    .unwrap()["number"]
            })
            .collect();
        return json!({"claims":[{"text":"已完成复核，工作库留有备份记录","sources":ids}]});
    }
    let sources: Vec<_> = ["发布暂缓", "负责人是林"]
        .iter()
        .map(|needle| {
            evidence
                .iter()
                .find(|e| e["excerpt"].as_str().unwrap().contains(needle))
                .unwrap()["number"]
                .clone()
        })
        .collect();
    json!({"claims":[{"text":"发布暂缓，由林跟进","sources":sources}]})
}

#[test]
fn knowledge_round_trip_search_citations_watch_and_restart() {
    use KnowledgeOperation as Op;
    let dir = std::env::temp_dir().join(format!("waku-knowledge-{}", Uuid::new_v4()));
    let notes = dir.join("notes");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("decision.md"), "# 决策\n发布暂缓，等待质量复核").unwrap();
    fs::write(notes.join("owners.md"), "# 分工\n负责人是林").unwrap();
    fs::write(notes.join("backup.md"), "# 备份\n单号 KB-42，保留三天").unwrap();
    let (listener, mut service) = http::mock_service("Library test", "dialogue");
    let address = listener.local_addr().unwrap();
    let mut encoder = ServiceModel::new("encoder".into());
    encoder.options.kind = ModelKind::Embedding;
    service.models.push(encoder.clone());
    encoder.id = "encoder-v2".into();
    service.models.push(encoder);
    let service_id = service.id;
    let select = |model: &str| KnowledgeModel {
        service_id,
        model: model.into(),
    };
    let config = LibraryConfig {
        root: notes.to_str().unwrap().into(),
        embedding: select("encoder"),
    };
    let chat = select("dialogue");
    let next_encoder = select("encoder-v2");
    DaemonSettingsStore::open(dir.join("settings.json"))
        .unwrap()
        .save_model_service(service, None)
        .unwrap();
    let indexed = Arc::new(AtomicUsize::new(0));
    let observed = indexed.clone();
    let queries = Arc::new(AtomicUsize::new(0));
    let observed_queries = queries.clone();
    let provider = thread::spawn(move || {
        loop {
            let (socket, route, body, _) = http::request(&listener);
            match route.as_str() {
                "POST /v1/embeddings" => {
                    let texts = body["input"].as_array().unwrap();
                    // Simulate one incompatible model response while the other libraries stay searchable
                    if body["model"] == "encoder-v2" && texts[0] == "部分模型故障" {
                        http::send(socket, "application/json", r#"{"data":[]}"#);
                        continue;
                    }
                    let data: Vec<_> = texts
                        .iter()
                        .enumerate()
                        .rev()
                        .map(|(i, t)| {
                            let text = t.as_str().unwrap();
                            if text.contains(".md\n") {
                                observed.fetch_add(1, Ordering::Relaxed);
                            } else {
                                observed_queries.fetch_add(1, Ordering::Relaxed);
                            }
                            let mut vector = embedding(text).to_vec();
                            if body["model"] == "encoder-v2" {
                                vector.push(0.0);
                            }
                            json!({"index":i, "embedding":vector})
                        })
                        .collect();
                    http::send(
                        socket,
                        "application/json",
                        &json!({"data":data}).to_string(),
                    );
                }
                "POST /v1/chat/completions" => {
                    http::stream(socket, json!({"content":claims(&body).to_string()}), "stop")
                }
                "GET /stop" => break,
                _ => panic!("unexpected model route {route}"),
            }
        }
    });
    // Start with the single-library file written by existing installations
    let original_root = notes.canonicalize().unwrap().to_str().unwrap().to_owned();
    let legacy = LibraryConfig {
        root: original_root.clone(),
        ..config.clone()
    };
    crate::settings::write_atomic(&dir.join("knowledge.json"), &legacy).unwrap();
    let (client, server) = start_daemon(&dir);
    let KnowledgeResult::Status { status } = call(&client, Op::Status) else {
        panic!()
    };
    assert_eq!(status.libraries, [legacy]);
    let stamp = ready(&client, 3, "");
    let search = |query: &str| {
        let KnowledgeResult::Matches { sources, .. } = call(
            &client,
            Op::Search {
                query: query.into(),
            },
        ) else {
            panic!()
        };
        sources
    };
    let start = Instant::now();
    let hit = &search("交付为什么延期")[0];
    assert_eq!(hit.path, "decision.md");
    assert_eq!(search("请查询KB-42的处理情况")[0].path, "backup.md");
    let read = || Op::Read {
        id: hit.document_id,
        revision: Some(hit.revision.clone()),
        start: hit.start,
        end: Some(hit.end),
    };
    let KnowledgeResult::Passage { text, changed, .. } = call(&client, read()) else {
        panic!()
    };
    assert!(!changed);
    assert_eq!(text, hit.text);
    let ask = |query: &str| {
        let KnowledgeResult::Answer { text, sources, .. } = call(
            &client,
            Op::Ask {
                query: query.into(),
                chat: chat.clone(),
            },
        ) else {
            panic!()
        };
        (text, sources)
    };
    let (text, sources) = ask("延期由谁跟进");
    assert_eq!(text, "发布暂缓，由林跟进 [1] [2]");
    assert_eq!(
        sources.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
        ["decision.md", "owners.md"]
    );
    assert!(
        cited_answer(
            r#"{"claims":[{"text":"invented","sources":[99]}]}"#,
            &sources
        )
        .is_err()
    );
    let (text, sources) = ask("预算多少");
    assert!(text.is_empty() && sources.is_empty());
    eprintln!(
        "two searches, two grounded queries and one source read over loopback: {:?}",
        start.elapsed()
    );
    fs::write(notes.join("decision.md"), "# 新决策\n复核完成").unwrap();
    let KnowledgeResult::Passage { changed, text, .. } = call(&client, read()) else {
        panic!()
    };
    assert!(changed && text.contains("复核完成"));
    let changed_stamp = ready(&client, 3, &stamp);
    fs::remove_file(notes.join("backup.md")).unwrap();
    ready(&client, 2, &changed_stamp);
    assert!(search("KB-42").iter().all(|s| s.path != "backup.md"));
    let before_restart = indexed.load(Ordering::Relaxed);
    let list_tree = || Op::List {
        offset: 0,
        include_folders: Some(true),
    };
    call(
        &client,
        Op::Manage {
            action: LibraryAction::CreateFolder {
                path: "项目/会议".into(),
            },
        },
    );
    assert_eq!(fs::read_dir(notes.join("项目/会议")).unwrap().count(), 0);
    assert!(
        matches!(call(&client, list_tree()), KnowledgeResult::Documents {folders, documents, ..}
        if folders == ["项目", "项目/会议"] && documents.len() == 2)
    );
    call(
        &client,
        Op::ManageTree {
            root: original_root.clone(),
            id: Some(hit.document_id),
            action: LibraryAction::Reorder {
                path: "decision.md".into(),
                sibling: "项目".into(),
                after: false,
            },
        },
    );
    assert!(
        matches!(call(&client, list_tree()),KnowledgeResult::Documents{positions,..} if positions["decision.md"] < positions["项目"])
    );
    assert_eq!(indexed.load(Ordering::Relaxed), before_restart);
    client.shutdown();
    server.join().unwrap();

    // Reopening reconciles offline edits while keeping unchanged vectors and document identity
    fs::rename(notes.join("owners.md"), notes.join("team.md")).unwrap();
    let (client, server) = start_daemon(&dir);
    ready(&client, 2, "");
    assert!(
        matches!(call(&client, list_tree()), KnowledgeResult::Documents {folders, positions, ..}
        if folders == ["项目", "项目/会议"] && positions["decision.md"] < positions["项目"])
    );
    let KnowledgeResult::Documents { documents, .. } = call(
        &client,
        Op::List {
            offset: 0,
            include_folders: None,
        },
    ) else {
        panic!()
    };
    assert_eq!(
        documents
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>(),
        ["decision.md", "team.md"]
    );
    assert_eq!(documents[0].id, hit.document_id);
    let before_switch = indexed.load(Ordering::Relaxed);
    assert_eq!(before_switch, before_restart + 1);
    call(
        &client,
        Op::Configure {
            config: LibraryConfig {
                embedding: next_encoder,
                ..config
            },
        },
    );
    ready(&client, 2, "");
    assert_eq!(indexed.load(Ordering::Relaxed), before_switch + 2);
    let KnowledgeResult::Matches { sources, .. } = call(
        &client,
        Op::Search {
            query: "谁跟进".into(),
        },
    ) else {
        panic!()
    };
    assert_eq!(sources[0].path, "team.md");
    call(&client, Op::Rebuild);
    ready(&client, 2, "");
    assert_eq!(indexed.load(Ordering::Relaxed), before_switch + 4);
    // Two saved roots may contain the same relative path but keep their own IDs, vectors and models
    let second = dir.join("work");
    fs::create_dir_all(&second).unwrap();
    fs::write(second.join("decision.md"), "# 备份\n工作库专用的备份记录").unwrap();
    let second = second.canonicalize().unwrap().to_str().unwrap().to_owned();
    // The third library is indexed without ever becoming the active library
    let third = dir.join("archive");
    fs::create_dir(&third).unwrap();
    fs::write(third.join("older.md"), "# 往期归档").unwrap();
    let third = third.canonicalize().unwrap().to_str().unwrap().to_owned();
    let KnowledgeResult::Status { status } = call(
        &client,
        Op::AddLibraries {
            roots: vec![
                second.clone(),
                third.clone(),
                original_root.clone(),
                second.clone(),
            ],
            embedding: select("encoder"),
        },
    ) else {
        panic!()
    };
    assert_eq!(status.libraries.len(), 3);
    assert_eq!(status.config.unwrap().root, second);
    assert_eq!(status.libraries[2].embedding.model, "encoder-v2");
    ready(&client, 1, "");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let KnowledgeResult::Matches { sources, .. } = call(
            &client,
            Op::Search {
                query: "往期归档".into(),
            },
        ) else {
            panic!()
        };
        if sources.iter().any(|s| s.library == "archive") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "inactive library was never indexed"
        );
        thread::sleep(Duration::from_millis(40));
    }
    let before_query = queries.load(Ordering::Relaxed);
    let KnowledgeResult::Matches { sources, warnings } = call(
        &client,
        Op::Search {
            query: "备份".into(),
        },
    ) else {
        panic!()
    };
    assert!(warnings.is_empty());
    assert_eq!(queries.load(Ordering::Relaxed) - before_query, 2);
    assert_eq!(sources.len(), 4);
    assert_eq!(sources[0].library, "work");
    assert_ne!(sources[0].document_id, hit.document_id);
    assert!(sources[0].text.contains("工作库专用"));
    let KnowledgeResult::Matches { sources, warnings } = call(
        &client,
        Op::Search {
            query: "部分模型故障".into(),
        },
    ) else {
        panic!()
    };
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("notes"));
    assert!(sources.iter().any(|s| s.library == "work"));
    assert!(sources.iter().all(|s| s.library != "notes"));
    // Editing an inactive folder refreshes its index while the displayed library stays the same
    fs::write(Path::new(&third).join("older.md"), "# 往期归档\n已补录").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let KnowledgeResult::Matches { sources, .. } = call(
            &client,
            Op::Search {
                query: "已补录".into(),
            },
        ) else {
            panic!()
        };
        if sources
            .iter()
            .any(|s| s.library == "archive" && s.text.contains("已补录"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "inactive library edits were not indexed"
        );
        thread::sleep(Duration::from_millis(40));
    }
    let KnowledgeResult::Answer {
        text,
        sources,
        warnings,
    } = call(
        &client,
        Op::Ask {
            query: "跨库核对决策和备份".into(),
            chat: chat.clone(),
        },
    )
    else {
        panic!()
    };
    assert!(warnings.is_empty());
    assert_eq!(text, "已完成复核，工作库留有备份记录 [1] [2]");
    assert_eq!(
        sources
            .iter()
            .map(|s| (s.library.as_str(), s.path.as_str()))
            .collect::<Vec<_>>(),
        [("notes", "decision.md"), ("work", "decision.md")]
    );
    assert_eq!(sources[0].document_id, hit.document_id);
    assert!(
        matches!(call(&client, read()), KnowledgeResult::Passage {text, ..} if text.contains("复核完成"))
    );
    assert!(
        matches!(call(&client, Op::Status), KnowledgeResult::Status {status} if status.config.as_ref().unwrap().root == second)
    );
    let count = indexed.load(Ordering::Relaxed);
    client.shutdown();
    server.join().unwrap();

    let (client, server) = start_daemon(&dir);
    let KnowledgeResult::Status { status } = call(&client, Op::Status) else {
        panic!()
    };
    assert_eq!(status.config.unwrap().root, second);
    assert_eq!(status.libraries.len(), 3);
    let KnowledgeResult::Status { status } = call(
        &client,
        Op::ActivateDocumentLibrary {
            id: hit.document_id,
        },
    ) else {
        panic!()
    };
    assert!(!status.indexing);
    assert_eq!(status.config.unwrap().root, original_root);
    assert!(
        matches!(call(&client, Op::DocumentFile { id: hit.document_id, action: DocumentFileAction::Source }), KnowledgeResult::Source {text, ..} if text.contains("复核完成"))
    );
    ready(&client, 2, "");
    let KnowledgeResult::Matches { sources, .. } = call(
        &client,
        Op::Search {
            query: "谁跟进".into(),
        },
    ) else {
        panic!()
    };
    assert_eq!(sources[0].path, "team.md");
    assert_eq!(indexed.load(Ordering::Relaxed), count);
    // Switching back picks up an offline edit without embedding the unchanged note again
    call(
        &client,
        Op::ActivateLibrary {
            root: second.clone(),
        },
    );
    ready(&client, 1, "");
    fs::write(notes.join("decision.md"), "# 决策\n已确认交付日期").unwrap();
    call(
        &client,
        Op::ActivateLibrary {
            root: original_root.clone(),
        },
    );
    ready(&client, 2, "");
    assert_eq!(indexed.load(Ordering::Relaxed), count + 1);
    let KnowledgeResult::Status { status } = call(
        &client,
        Op::RemoveLibrary {
            root: original_root,
        },
    ) else {
        panic!()
    };
    assert_eq!(status.config.unwrap().root, second);
    let KnowledgeResult::Matches { sources, .. } = call(
        &client,
        Op::Search {
            query: "决策".into(),
        },
    ) else {
        panic!()
    };
    assert!(sources.iter().all(|s| s.library != "notes"));
    for operation in [
        read(),
        Op::ActivateDocumentLibrary {
            id: hit.document_id,
        },
    ] {
        assert!(
            client
                .request(Uuid::nil(), Uuid::nil(), Command::Knowledge { operation })
                .is_err()
        );
    }
    call(&client, Op::RemoveLibrary { root: third });
    let KnowledgeResult::Status { status } = call(
        &client,
        Op::RemoveLibrary {
            root: second.clone(),
        },
    ) else {
        panic!()
    };
    assert!(status.libraries.is_empty() && status.config.is_none());
    assert!(
        notes.join("decision.md").is_file() && Path::new(&second).join("decision.md").is_file()
    );
    assert!(
        catalog::load(&dir.join("knowledge.json"))
            .unwrap()
            .is_empty()
    );
    client.shutdown();
    server.join().unwrap();
    write!(
        TcpStream::connect(address).unwrap(),
        "GET /stop HTTP/1.1\r\nHost: localhost\r\n\r\n"
    )
    .unwrap();
    provider.join().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn knowledge_chunk_offsets_survive_fences_headings_and_long_unicode_lines() {
    let text = format!(
        "# 容器\r\n## 子标题\r\n\n```md\n# 示例标题\n```\n\n{}",
        "中😀".repeat(1600)
    );
    let parts = index::chunks(&text);
    for p in &parts {
        assert_eq!(p.body, &text[p.start..p.end]);
        assert_eq!(
            p.line,
            1 + text[..p.start].bytes().filter(|b| *b == b'\n').count()
        );
        assert!(p.body.chars().count() <= 1200);
    }
    assert!(parts.last().unwrap().heading.ends_with("子标题"));
    assert_eq!(
        parts
            .iter()
            .map(|p| p.body.matches('😀').count())
            .sum::<usize>(),
        1600
    );
}

/// The caller supplies a corpus explicitly, source notes are only opened for reading
#[test]
#[ignore = "requires WAKU_TEST_LIBRARY"]
fn knowledge_readonly_corpus_offsets() {
    let root = PathBuf::from(std::env::var_os("WAKU_TEST_LIBRARY").unwrap())
        .canonicalize()
        .unwrap();
    let start = Instant::now();
    let files = index::inventory(&root).unwrap().files;
    let mut passages = 0;
    for path in &files {
        let text = index::read(&root, path).unwrap();
        let parts = index::chunks(&text);
        for p in &parts {
            assert_eq!(p.body, &text[p.start..p.end]);
            assert_eq!(
                p.line,
                1 + text[..p.start].bytes().filter(|b| *b == b'\n').count()
            );
        }
        passages += parts.len();
    }
    assert!(passages > 0);
    eprintln!(
        "checked {} Markdown files and {passages} source ranges in {:?}",
        files.len(),
        start.elapsed()
    );
}

/// Explicit opt-in exercises saved model services with a disposable index and a specified test corpus
#[test]
#[ignore = "sends the supplied WAKU_TEST_LIBRARY and WAKU_TEST_QUESTION to the selected model services"]
fn knowledge_live_models() -> Result<()> {
    let root = std::env::var("WAKU_TEST_LIBRARY")?;
    let query = std::env::var("WAKU_TEST_QUESTION")?;
    let settings = Arc::new(DaemonSettingsStore::open(
        crate::DaemonSettings::default_path(),
    )?);
    let saved = settings.get();
    let selected = |name: &str| -> Result<KnowledgeModel> {
        let model = std::env::var(name)?;
        let service = saved
            .model_services
            .iter()
            .find(|s| s.enabled && s.model(&model).is_some())
            .context("Missing test model")?;
        Ok(KnowledgeModel {
            service_id: service.id,
            model,
        })
    };
    let embedding = selected("WAKU_TEST_EMBEDDING")?;
    let chat = selected("WAKU_TEST_CHAT")?;
    let dir = std::env::temp_dir().join(format!("waku-live-knowledge-{}", Uuid::new_v4()));
    fs::create_dir(&dir)?;
    let lib = KnowledgeLibrary::open(dir.join("knowledge.sqlite"), settings)?;
    let result = (|| -> Result<()> {
        let start = Instant::now();
        lib.execute(KnowledgeOperation::Configure {
            config: LibraryConfig { root, embedding },
        })?;
        loop {
            let KnowledgeResult::Status { status } = lib.execute(KnowledgeOperation::Status)?
            else {
                unreachable!()
            };
            if let Some(error) = status.error {
                bail!("{error}");
            }
            if !status.indexing && status.ready == status.total && status.ready > 0 {
                eprintln!(
                    "indexed {} documents in {:?}",
                    status.ready,
                    start.elapsed()
                );
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(300),
                "Indexing timed out"
            );
            thread::sleep(Duration::from_millis(100));
        }
        let start = Instant::now();
        let KnowledgeResult::Matches { sources, .. } = lib.execute(KnowledgeOperation::Search {
            query: query.clone(),
        })?
        else {
            unreachable!()
        };
        let first = sources.first().context("No passages retrieved")?;
        eprintln!(
            "search {:?}, first result {} L{}",
            start.elapsed(),
            first.path,
            first.line
        );
        let start = Instant::now();
        let KnowledgeResult::Answer { text, sources, .. } =
            lib.execute(KnowledgeOperation::Ask {
                query,
                chat: chat.clone(),
            })?
        else {
            unreachable!()
        };
        ensure!(
            !text.is_empty() && !sources.is_empty(),
            "Question received no supported answer"
        );
        if let Ok(term) = std::env::var("WAKU_TEST_EVIDENCE") {
            ensure!(
                sources.iter().any(|s| s.text.contains(&term)),
                "No citation contains the expected supporting term"
            );
        }
        for source in &sources {
            eprintln!("cited {} L{}", source.path, source.line);
            let KnowledgeResult::Passage { text, changed, .. } =
                lib.execute(KnowledgeOperation::Read {
                    id: source.document_id,
                    revision: Some(source.revision.clone()),
                    start: source.start,
                    end: Some(source.end),
                })?
            else {
                unreachable!()
            };
            ensure!(!changed && text == source.text, "Cited source changed");
        }
        eprintln!(
            "grounded answer {:?}, {} verified citations",
            start.elapsed(),
            sources.len()
        );
        if let Ok(unsupported) = std::env::var("WAKU_TEST_UNANSWERABLE") {
            let KnowledgeResult::Answer { text, sources, .. } =
                lib.execute(KnowledgeOperation::Ask {
                    query: unsupported,
                    chat,
                })?
            else {
                unreachable!()
            };
            ensure!(
                text.is_empty() && sources.is_empty(),
                "The model answered an unsupported question"
            );
            eprintln!("unsupported question returned no claims or citations");
        }
        Ok(())
    })();
    drop(lib);
    fs::remove_dir_all(dir)?;
    result
}

/// Incomplete work stays private to the indexing connection, including across process restarts
#[test]
fn knowledge_incomplete_revision_is_never_retrieved() {
    let dir = std::env::temp_dir().join(format!("waku-stage-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("index.sqlite");
    let mut db = index::initialize(&path).unwrap();
    let (id, _) = index::prepare(&db, "/notes", "doc.md", "first", "model").unwrap();
    index::begin_document(&db).unwrap();
    index::stage(&mut db, &index::chunks("before"), &[vec![1.0]]).unwrap();
    index::publish(&mut db, &id, "doc.md", "first", "model").unwrap();
    index::prepare(&db, "/notes", "doc.md", "second", "model").unwrap();
    index::begin_document(&db).unwrap();
    index::stage(&mut db, &index::chunks("after"), &[vec![1.0]]).unwrap();
    assert!(
        index::search(
            &index::connect(&path).unwrap(),
            "/notes",
            "model",
            "after",
            &[1.0]
        )
        .unwrap()
        .is_empty()
    );
    drop(db);
    let mut db = index::initialize(&path).unwrap();
    assert_eq!(
        index::prepare(&db, "/notes", "doc.md", "second", "model").unwrap(),
        (id.clone(), false)
    );
    index::begin_document(&db).unwrap();
    index::stage(&mut db, &index::chunks("complete"), &[vec![1.0]]).unwrap();
    index::publish(&mut db, &id, "doc.md", "second", "model").unwrap();
    let hits = index::search(&db, "/notes", "model", "after", &[1.0]).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1.text, "complete");
    drop(db);
    fs::remove_dir_all(dir).unwrap();
}

/// Isolate filesystem and catalog mutations from the asynchronous embedding worker
fn tree_fixture() -> Result<(tempfile::TempDir, PathBuf, KnowledgeLibrary)> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().canonicalize()?.join("notes");
    fs::create_dir(&root)?;
    let (wake, _) = bounded(1);
    let config = LibraryConfig {
        root: root.to_str().unwrap().into(),
        embedding: KnowledgeModel {
            service_id: Uuid::new_v4(),
            model: "fixture".into(),
        },
    };
    let lib = KnowledgeLibrary {
        path: tmp.path().join("index.sqlite"),
        settings: Arc::new(DaemonSettingsStore::open(tmp.path().join("settings.json"))?),
        wake,
        writes: Mutex::new(()),
        state: Mutex::new(State {
            status: LibraryStatus {
                config: Some(config.clone()),
                libraries: vec![config],
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    Ok((tmp, root, lib))
}

/// Creation reserves names on disk and returns the exact document and revision to edit
#[test]
fn untitled_entries_create_in_place_and_preserve_existing_siblings() -> Result<()> {
    use KnowledgeOperation as Op;
    let (_tmp, root, library) = tree_fixture()?;
    let create = |parent: &str, stem: &str, folder| {
        library.execute(Op::CreateUntitled {
            root: root.to_str().unwrap().into(),
            parent: parent.into(),
            stem: stem.into(),
            folder,
        })
    };
    for expected in ["未命名文件夹", "未命名文件夹 2"] {
        assert!(
            matches!(create("", "未命名文件夹", true)?, KnowledgeResult::Managed { paths } if paths == [expected])
        );
        assert!(root.join(expected).is_dir());
    }
    let parent = "未命名文件夹";
    fs::write(root.join(parent).join("untitled.md"), "existing")?;
    let (first, second) = thread::scope(|scope| {
        let first = scope.spawn(|| create(parent, "untitled", false));
        (create(parent, "untitled", false), first.join().unwrap())
    });
    let mut paths = Vec::new();
    for created in [first?, second?] {
        let KnowledgeResult::CreatedDocument { id, path, revision } = created else {
            panic!("expected an editable document")
        };
        assert_eq!(fs::read(root.join(&path))?, b"");
        library.execute(Op::ManageTree {
            root: root.to_str().unwrap().into(),
            id: Some(id),
            action: LibraryAction::Update {
                path: path.clone(),
                content: "# 标题\n\n正文".into(),
                revision: Some(revision),
                mode: UpdateMode::Replace,
            },
        })?;
        assert_eq!(fs::read_to_string(root.join(&path))?, "# 标题\n\n正文");
        paths.push(path);
    }
    paths.sort();
    assert_eq!(
        paths,
        ["未命名文件夹/untitled 2.md", "未命名文件夹/untitled 3.md"]
    );
    assert_eq!(
        fs::read_to_string(root.join(parent).join("untitled.md"))?,
        "existing"
    );
    assert!(
        matches!(create("", "無題", false)?, KnowledgeResult::CreatedDocument { path, .. } if path == "無題.md")
    );
    assert!(create("missing", "untitled", true).is_err());
    assert!(create("", "nested/name", false).is_err());
    Ok(())
}

/// Moving a directory preserves notes, attachments, empty children and historical citation IDs
#[test]
fn knowledge_tree_moves_subtrees_and_persists_sibling_order() -> Result<()> {
    use LibraryAction as A;
    let (_tmp, root, lib) = tree_fixture()?;
    let root_name = root.to_str().unwrap();
    let run = |action| lib.execute(KnowledgeOperation::Manage { action });
    let folder = |s: &str| run(A::CreateFolder { path: s.into() });
    let create = |p: &str, body: &str| {
        run(A::Create {
            path: p.into(),
            content: body.into(),
            metadata: None,
            format: None,
        })
    };
    let reorder = |p: &str, s: &str, after| {
        run(A::Reorder {
            path: p.into(),
            sibling: s.into(),
            after,
        })
    };
    let list = |dir: &str| -> Result<Vec<String>> {
        Ok(lib.list_entries(dir, false, 0)?["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_owned())
            .collect())
    };
    folder("会议/子目录/空")?;
    folder("归档")?;
    folder("会议记录")?;
    create("会议/a.md", "[指南](../index.md) ![图](image.png)")?;
    create("会议/子目录/b.md", "需要保留的原文")?;
    create("index.md", "[纪要](会议/a.md#section)")?;
    fs::write(root.join("会议/image.png"), b"attachment")?;
    let id = lib.document_id("会议/子目录/b.md")?;
    let rev = index::revision("需要保留的原文");
    let before = lib.state.lock().generation;
    let db = index::connect(&lib.path)?;
    db.execute("UPDATE documents SET indexed_with='ready'", [])?;
    reorder("会议/a.md", "会议/子目录", false)?;
    reorder("index.md", "会议", false)?;
    assert_eq!(list("会议")?, ["会议/a.md", "会议/子目录/"]);
    assert_eq!(lib.state.lock().generation, before);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM documents WHERE indexed_with='ready'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        3
    );
    assert!(
        matches!(index::list(&index::initialize(&lib.path)?,root_name,0,true)?,KnowledgeResult::Documents{positions,..}
        if positions["会议/a.md"] < positions["会议/子目录"])
    );
    // The sibling target determines the new parent and insertion point in one operation
    create("归档/end.md", "end")?;
    assert!(reorder("会议", "不存在/anchor.md", false).is_err());
    assert!(!root.join("不存在").exists());
    reorder("会议", "归档/end.md", true)?;
    // A vector batch staged under the old path must never replace the moved note's passages
    let mut worker = index::connect(&lib.path)?;
    index::begin_document(&worker)?;
    index::stage(&mut worker, &index::chunks("需要保留的原文"), &[vec![1.0]])?;
    assert!(
        index::publish(
            &mut worker,
            &id.to_string(),
            "会议/子目录/b.md",
            &rev,
            "old-model"
        )
        .is_err()
    );
    assert_eq!(list("归档")?, ["归档/end.md", "归档/会议/"]);
    assert_eq!(list("归档/会议")?, ["归档/会议/a.md", "归档/会议/子目录/"]);
    assert!(root.join("归档/会议/子目录/空").is_dir());
    assert_eq!(fs::read(root.join("归档/会议/image.png"))?, b"attachment");
    assert!(root.join("会议记录").is_dir());
    assert_eq!(
        fs::read_to_string(root.join("归档/会议/a.md"))?,
        "[指南](../../index.md) ![图](image.png)"
    );
    let inbound = fs::read_to_string(root.join("index.md"))?;
    assert!(inbound.contains("a.md#section") && inbound != "[纪要](会议/a.md#section)");
    let url = url::Url::from_file_path(root.join("index.md")).unwrap();
    assert_eq!(
        url.join(inbound.split_once('(').unwrap().1.trim_end_matches(')'))?
            .to_file_path()
            .unwrap(),
        root.join("归档/会议/a.md")
    );
    assert_eq!(lib.document_id("归档/会议/子目录/b.md")?, id);
    assert!(
        matches!(lib.execute(KnowledgeOperation::Read{id,revision:Some(rev),start:0,end:None})?,
        KnowledgeResult::Passage{path,changed:false,..} if path=="归档/会议/子目录/b.md")
    );
    let move_to = |p: &str, d: &str| {
        run(A::Move {
            from: p.into(),
            to: d.into(),
        })
    };
    assert!(move_to("归档/会议", "归档/会议/子目录/loop").is_err());
    assert!(move_to("归档/会议", "会议记录").is_err());
    assert!(reorder("归档/会议", "归档/会议/子目录", true).is_err());
    move_to("归档/会议", "新会议")?;
    assert_eq!(lib.document_id("新会议/子目录/b.md")?, id);
    run(A::DeleteFolder {
        path: "新会议".into(),
    })?;
    assert!(lib.document_id("新会议/子目录/b.md").is_err());
    assert!(list("")?.iter().all(|p| p != "新会议/"));
    let trash = fs::read_dir(root.join(".waku-trash"))?
        .next()
        .unwrap()?
        .path();
    assert!(trash.join("新会议/子目录/空").is_dir() && trash.join("新会议/image.png").is_file());
    Ok(())
}

/// Exercise the same mutations the desktop and assistant can issue, in a disposable directory
#[test]
fn knowledge_management_versions_links_tags_and_recovery() -> Result<()> {
    use waku_protocol::knowledge::{
        KnowledgeOperation as O, KnowledgeResult as R, LibraryAction as A,
    };
    let (tmp, root, lib) = tree_fixture()?;
    let op = |action| lib.execute(O::Manage { action });
    let folder = |path: &str| op(A::CreateFolder { path: path.into() });
    folder("项目/会议")?;
    assert!(folder("项目/会议").is_err());
    for invalid in ["", "../outside", ".hidden", "项目//季度"] {
        assert!(folder(invalid).is_err());
    }
    let create = |path: &str, text: &str| {
        op(A::Create {
            path: path.into(),
            content: text.into(),
            metadata: None,
            format: None,
        })
    };
    create("ideas/a.md", "# Topic\n\n* Hello\n")?;
    create(
        "index.md",
        "[open](ideas/a.md#topic)\n\n`[code](ideas/a.md)`",
    )?;
    assert!(create("../outside.md", "invalid").is_err());
    assert!(create("ideas/./a.md", "invalid").is_err());
    assert!(create("ideas//a.md", "invalid").is_err());
    assert!(create("ideas/a.md", "collision").is_err());
    assert!(folder("ideas/a.md").is_err());
    let before = lib.document_id("ideas/a.md")?;
    // A row operation targets its own ID even across moves and a replacement at the old path
    let file = |id, action| lib.execute(O::DocumentFile { id, action });
    file(
        before,
        DocumentFileAction::Duplicate {
            to: "ideas/copy.md".into(),
        },
    )?;
    let copy_id = lib.document_id("ideas/copy.md")?;
    assert_ne!(before, copy_id);
    file(
        copy_id,
        DocumentFileAction::Move {
            to: "moved/duplicate.md".into(),
        },
    )?;
    assert!(
        matches!(file(copy_id, DocumentFileAction::Source)?, R::Source { path, text, .. }
        if path == "moved/duplicate.md" && text == "# Topic\n\n* Hello\n")
    );
    assert!(
        file(
            copy_id,
            DocumentFileAction::Move {
                to: "ideas/a.md".into()
            }
        )
        .is_err()
    );
    file(copy_id, DocumentFileAction::Delete)?;
    create("moved/duplicate.md", "replacement")?;
    assert!(file(copy_id, DocumentFileAction::Delete).is_err());
    assert_eq!(
        fs::read_to_string(root.join("moved/duplicate.md"))?,
        "replacement"
    );
    op(A::Update {
        path: "ideas/a.md".into(),
        content: "\nMore\n".into(),
        mode: UpdateMode::Append,
        revision: None,
    })?;
    op(A::BatchTag {
        paths: vec!["index.md".into(), "ideas/a.md".into()],
        tags: vec!["work".into()],
        operation: TagMode::Add,
    })?;
    let original = fs::read_to_string(root.join("index.md"))?;
    assert!(original.contains("work"));
    assert!(
        op(A::BatchTag {
            paths: vec!["index.md".into(), "missing.md".into()],
            tags: vec!["uncommitted".into()],
            operation: TagMode::Add
        })
        .is_err()
    );
    assert_eq!(fs::read_to_string(root.join("index.md"))?, original);
    op(A::Move {
        from: "ideas/a.md".into(),
        to: "projects/b.md".into(),
    })?;
    assert_eq!(lib.document_id("projects/b.md")?, before);
    assert!(lib.versions(root.to_str().unwrap(), "projects/b.md")?.len() >= 3);
    let links = fs::read_to_string(root.join("index.md"))?;
    assert!(
        links.contains("[open](projects/b.md#topic)") && links.contains("`[code](ideas/a.md)`"),
        "{links}"
    );
    let texts = lib.list_entries("projects", false, 0)?;
    assert_eq!(texts["documents"][0]["path"], "projects/b.md");
    assert_eq!(texts["documents"][0]["tags"][0], "work");
    assert_eq!(texts["documents"][0]["title"], "Topic");
    op(A::Delete {
        path: "projects/b.md".into(),
    })?;
    assert!(!root.join("projects/b.md").exists());
    let R::Versions { versions } = lib.execute(O::History {
        path: "projects/b.md".into(),
    })?
    else {
        unreachable!()
    };
    op(A::Restore {
        path: "projects/b.md".into(),
        version: versions[0].version.clone(),
    })?;
    assert!(root.join("projects/b.md").is_file());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(tmp.path(), root.join("escape"))?;
        assert!(create("escape/outside.md", "invalid").is_err());
        assert!(folder("escape/outside").is_err());
        assert!(!tmp.path().join("outside").exists());
        assert!(!tmp.path().join("outside.md").exists());
    }
    Ok(())
}

/// A saved assistant and a normal conversation use real model tool requests against a private copy of the test corpus
#[test]
#[ignore = "uses explicitly selected live model services and WAKU_TEST_LIBRARY"]
fn knowledge_agent_live() -> Result<()> {
    live_assistant_session(false)
}

/// Web import uses a fresh ordinary assistant session and an empty test library
#[test]
#[ignore = "contacts the selected model and the public example.com HTTPS fixture"]
fn knowledge_agent_live_public_import() -> Result<()> {
    live_assistant_session(true)
}

fn live_assistant_session(import_only: bool) -> Result<()> {
    use crate::driver::{self, assistant};
    use crate::model::DriverEvent;
    use waku_protocol::settings::AssistantProfile;
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("notes");
    fs::create_dir(&root)?;
    if !import_only {
        let corpus = PathBuf::from(
            std::env::var_os("WAKU_TEST_LIBRARY").context("Missing WAKU_TEST_LIBRARY")?,
        )
        .canonicalize()?;
        for path in index::inventory(&corpus)?.files {
            let dest = root.join(&path);
            fs::create_dir_all(dest.parent().unwrap())?;
            fs::write(dest, index::read(&corpus, &path)?)?;
        }
    }
    let original = DaemonSettingsStore::open(crate::DaemonSettings::default_path())?;
    let settings = Arc::new(DaemonSettingsStore::open(tmp.path().join("settings.json"))?);
    let select = |name: &str| -> Result<KnowledgeModel> {
        let model = std::env::var(name)?;
        let service = original
            .get()
            .model_services
            .into_iter()
            .find(|s| s.enabled && s.model(&model).is_some())
            .context("Test model missing")?;
        let id = service.id;
        let url = assistant::service_url(&service.base_url)?;
        let key = original.model_service_key(id, url.as_str())?;
        settings.save_model_service(service, key.as_deref())?;
        Ok(KnowledgeModel {
            model,
            service_id: id,
        })
    };
    let embedding = select("WAKU_TEST_EMBEDDING")?;
    let chat = select("WAKU_TEST_CHAT")?;
    let profile = AssistantProfile {
        id: Uuid::new_v4(),
        name: "Test assistant".into(),
        system_prompt: "你是我的个人助理，请用中文回答。".into(),
        model: chat.model,
        model_service_id: Some(chat.service_id),
        web_search: false,
        knowledge_tools: true,
        base_url: String::new(),
    };
    let mut cfg = settings.get();
    cfg.assistants.push(profile.clone());
    settings.replace(cfg)?;
    let lib = KnowledgeLibrary::open(tmp.path().join("knowledge.sqlite"), settings.clone())?;
    lib.execute(KnowledgeOperation::Configure {
        config: LibraryConfig {
            root: root.display().to_string(),
            embedding,
        },
    })?;
    let start = Instant::now();
    loop {
        let status = lib.state.lock().status.clone();
        if let Some(err) = status.error {
            bail!("{err}")
        }
        if !status.catalog_revision.is_empty() && !status.indexing && status.total == status.ready {
            break;
        }
        ensure!(
            start.elapsed() < Duration::from_secs(180),
            "Indexing timed out"
        );
        thread::sleep(Duration::from_millis(80));
    }
    let (events, receiver) = driver::test_event_channel();
    let agent = assistant::start(
        profile,
        assistant::TurnTools {
            knowledge: true,
            ..Default::default()
        },
        settings,
        Some(lib.clone()),
        crate::attachments::AttachmentStore::new(tmp.path().join("attachments")),
        Arc::new(crate::blob_store::BlobStore::new(tmp.path().join("blobs"))),
        vec![],
        events,
        None,
    )?;
    let run = |prompt: &str| -> Result<(HashSet<String>, String)> {
        let mut calls = HashSet::new();
        let mut answer = String::new();
        agent.prompt(prompt.into());
        loop {
            match receiver.recv_timeout(Duration::from_secs(125))? {
                DriverEvent::RichActivity(activity) if activity.complete && !activity.failed => {
                    if let Some(name) = activity.tool_name {
                        calls.insert(name);
                    }
                }
                DriverEvent::RichActivity(activity) if activity.failed => eprintln!(
                    "Failed tool {:?}: {:?}",
                    activity.tool_name, activity.output
                ),
                DriverEvent::TextDelta(text) => answer.push_str(&text),
                DriverEvent::Error(e) => bail!("{e}"),
                DriverEvent::TurnFinished { success, summary } => {
                    ensure!(success, "{summary:?}");
                    break;
                }
                _ => {}
            }
        }
        eprintln!("ordinary assistant tool calls: {calls:?}");
        Ok((calls, answer))
    };
    if import_only {
        let (used, answer) = run(
            "请用 knowledge_import_url 导入 https://example.com 到 verify/article.md，不下载图片，保留来源元数据。",
        )?;
        ensure!(
            used.contains("knowledge_import_url")
                && fs::read_to_string(root.join("verify/article.md"))?.contains("Example Domain"),
            "Missing imported article: {answer}"
        );
        return Ok(());
    }
    let (used, answer) =
        run("帮我在我的笔记中搜索关于 MySQL 最左前缀的资料。请引用原文链接回答。")?;
    ensure!(
        used.contains("knowledge_search") && answer.contains("waku-knowledge://"),
        "Missing semantic tool result or citation"
    );
    let (used, _) = run(
        "帮我新建一篇验收笔记，放在 verify/agent-proof.md，内容写 AGENT-RAG-42。请实际保存文件。",
    )?;
    ensure!(
        used.contains("knowledge_create")
            && fs::read_to_string(root.join("verify/agent-proof.md"))?.contains("AGENT-RAG-42"),
        "Missing created note"
    );
    let (used, answer) = run(
        "在 verify/agent-proof.md 末尾追加 VERIFIED，并给笔记添加 acceptance 标签。请实际保存更改。",
    )?;
    ensure!(
        used.contains("knowledge_update") && used.contains("knowledge_batch_tag"),
        "Missing update or tagging tool call: {answer}"
    );
    let text = fs::read_to_string(root.join("verify/agent-proof.md"))?;
    ensure!(
        text.contains("VERIFIED") && text.contains("acceptance"),
        "Missing persisted changes"
    );
    let (used, answer) = run(
        "请用 knowledge_list 查看 verify 目录，然后用 knowledge_read 打开 agent-proof.md 核实内容。",
    )?;
    ensure!(
        used.contains("knowledge_list") && used.contains("knowledge_read"),
        "Missing directory or read call: {answer}"
    );
    let (used, answer) = run("请使用移动工具将 verify/agent-proof.md 改名为 verify/final.md。")?;
    ensure!(
        used.contains("knowledge_move") && root.join("verify/final.md").is_file(),
        "Missing moved note: {answer}"
    );
    let (used, answer) =
        run("确认将临时验收笔记 verify/final.md 移入回收站，请立即调用 knowledge_delete 执行。")?;
    ensure!(
        used.contains("knowledge_delete") && !root.join("verify/final.md").exists(),
        "Missing trash operation: {answer}"
    );
    drop(agent);
    drop(lib);
    Ok(())
}
