//! Both knowledge readers scroll over the same complete Markdown document

use super::*;
use md::parser::IncrementalParser;

/// Keep Markdown structure and citation offsets together until the preview mounts
pub(super) struct PreparedDocument {
    parser: IncrementalParser,
    first_block: usize,
}

impl PreparedDocument {
    /// Citation offsets address the original file, including any hidden YAML header
    pub(super) fn new(text: &str, start: usize) -> Self {
        let body = split_front_matter(text).map_or(text, |(_, body)| body);
        let offset = start.saturating_sub(text.len() - body.len());
        let mut parser = IncrementalParser::new();
        parser.reset(body);
        let blocks = &parser.tree().blocks;
        let first_block = blocks
            .partition_point(|block| block.range.end <= offset)
            .min(blocks.len().saturating_sub(1));
        Self {
            parser,
            first_block,
        }
    }
}

/// Read by catalog identity so the revision, byte offsets and body all describe one snapshot
pub(super) fn load_preview(
    client: &waku_client::DaemonClient,
    id: Uuid,
    expected: Option<&str>,
    start: usize,
    end: Option<usize>,
) -> anyhow::Result<LoadedPreview> {
    let KnowledgeResult::Source {
        path,
        text,
        revision,
    } = request(
        client,
        KnowledgeOperation::DocumentFile {
            id,
            action: DocumentFileAction::Source,
        },
    )?
    else {
        anyhow::bail!(tr!("assistant.invalid_response"));
    };
    let changed = expected.is_some_and(|expected| expected != revision);
    let start = if changed {
        0
    } else {
        let end = end.unwrap_or(text.len());
        anyhow::ensure!(
            start <= end && text.is_char_boundary(start) && text.is_char_boundary(end),
            "Invalid passage boundary"
        );
        start
    };
    Ok(LoadedPreview {
        revision,
        id,
        path,
        changed,
        document: PreparedDocument::new(&text, start),
    })
}

/// Parsing happens once and only visible top-level blocks enter layout during scrolling
pub(super) struct DocumentPreview {
    view: Rc<MarkdownView>,
    rows: ListState,
    scrollbar: Rc<ScrollbarState>,
    selection: TranscriptSelection,
    math_menu: ContextMenuHandle,
    pub(super) focus: FocusHandle,
}

impl DocumentPreview {
    pub(super) fn new(document: PreparedDocument, cx: &mut App) -> Self {
        let count = document.parser.tree().blocks.len();
        let rows =
            ListState::new(count, ListAlignment::Top, px(128.)).with_uniform_item_height(px(40.));
        if document.first_block > 0 {
            rows.scroll_to(ListOffset {
                item_ix: document.first_block,
                offset_in_item: px(0.),
            });
        }
        Self {
            view: Rc::new(MarkdownView::from_parser(document.parser)),
            rows,
            scrollbar: ScrollbarState::new(),
            selection: Default::default(),
            math_menu: ContextMenuHandle::new(cx),
            focus: cx.focus_handle(),
        }
    }

    pub(super) fn render(&self, metrics: MarkdownMetrics, math: bool, cx: &App) -> AnyElement {
        let theme = Theme::current(cx);
        let palette = MarkdownPalette::from_theme(&theme);
        let view = self.view.clone();
        let selection = self.selection.clone();
        let menu = self.math_menu.clone();
        let link: md::render::LinkHandler = Rc::new(|url, _, cx| {
            if url.starts_with("https://") || url.starts_with("http://") {
                cx.open_url(url);
            }
        });
        let count = self.rows.item_count();
        let rows = list(self.rows.clone(), move |index, _, _| {
            let ctx = MarkdownCtx::new("knowledge-document", &palette, metrics, selection.clone())
                .with_math_enabled(math)
                .with_context_menu(menu.clone())
                .with_link_handler(link.clone());
            div()
                .w_full()
                .min_w_0()
                .px(px(16.))
                .pt(px(if index == 0 { 14. } else { metrics.block_gap }))
                .when(index + 1 == count, |row| row.pb(px(24.)))
                .child(view.document_block(index, &ctx))
                .into_any_element()
        });
        let selection = self.selection.clone();
        let keyboard_scroll = self.rows.clone();
        let content = div()
            .id("knowledge-document")
            .track_focus(&self.focus)
            .tab_index(0)
            .tab_stop(true)
            .focus_visible(|s| s.border_1().border_color(theme.accent))
            .on_key_down(move |event, window, cx| {
                match event.keystroke.key.as_str() {
                    "home" => keyboard_scroll.scroll_to(ListOffset::default()),
                    "end" => keyboard_scroll.scroll_to_end(),
                    "pageup" => {
                        keyboard_scroll.scroll_by(-keyboard_scroll.viewport_bounds().size.height)
                    }
                    "pagedown" => {
                        keyboard_scroll.scroll_by(keyboard_scroll.viewport_bounds().size.height)
                    }
                    "up" => keyboard_scroll.scroll_by(-px(metrics.line_height)),
                    "down" => keyboard_scroll.scroll_by(px(metrics.line_height)),
                    _ => return,
                }
                cx.stop_propagation();
                window.refresh();
            })
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .bg(theme.surface)
            .text_color(theme.text)
            .child(md::render::frame_reset(self.selection.clone()))
            .child(rows.size_full())
            .child(scrollbar::vertical(&self.rows, &self.scrollbar))
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| md::render::install_selection_input(window, &selection),
                )
                .absolute()
                .size(px(0.)),
            );
        context_menu(content, "knowledge-document-math", &self.math_menu, |_| {
            Vec::new()
        })
    }
}

/// Detached readers own their scroll position independently of the library panel
pub(super) struct DocumentWindow {
    pub(super) preview: DocumentPreview,
    pub(super) metrics: MarkdownMetrics,
    pub(super) math: bool,
}

impl Render for DocumentWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .child(self.preview.render(self.metrics, self.math, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same mounted reader reaches both ends without requesting another passage
    #[gpui::test]
    fn reader_scrolls_to_the_last_paragraph_and_back(cx: &mut gpui::TestAppContext) {
        let text = format!("笔记开头\n\n{}笔记末尾", "一个完整段落\n\n".repeat(1200));
        let (window, cx) = cx.add_window_view(|window, cx| {
            let preview = DocumentPreview::new(PreparedDocument::new(&text, 0), cx);
            window.focus(&preview.focus, cx);
            DocumentWindow {
                preview,
                metrics: MarkdownMetrics::BODY,
                math: false,
            }
        });
        let visible = |needle: &str, cx: &gpui::VisualTestContext| {
            window.read_with(cx, |reader, _| {
                reader
                    .preview
                    .selection
                    .registry
                    .borrow()
                    .entries()
                    .iter()
                    .any(|entry| entry.text.as_ref() == needle)
            })
        };
        cx.run_until_parked();
        assert!(visible("笔记开头", cx));
        assert!(!visible("笔记末尾", cx));
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        assert!(visible("笔记末尾", cx));
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert!(visible("笔记开头", cx));
    }

    /// A late citation and a code fence crossing the old passage limit share one complete parse
    #[test]
    fn full_document_preserves_blocks_and_citation_offsets() {
        let header = "---\ntitle: 笔记\n---\n";
        let body = format!(
            "# 开头\n\n```text\n{}```\n\n## 结尾\n\n最后一段\n",
            "代码行\n".repeat(4000)
        );
        let text = format!("{header}{body}");
        let offset = text.find("## 结尾").unwrap();
        let doc = PreparedDocument::new(&text, offset);
        let blocks = &doc.parser.tree().blocks;
        assert_eq!(doc.parser.text(), body);
        assert_eq!(blocks.len(), 4);
        assert!(matches!(
            blocks[1].block,
            md::parser::Block::CodeBlock { .. }
        ));
        assert_eq!(doc.first_block, 2);
        assert_eq!(blocks[doc.first_block].range.start, offset - header.len());
        assert_eq!(
            body[blocks.last().unwrap().range.clone()].trim(),
            "最后一段"
        );
    }
}
