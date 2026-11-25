//! Step 4：把译文回填进原始 EPUB 骨架并导出。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;

use anyhow::Result;
use chrono::Utc;
use regex::Regex;
use tracing::{info, warn};
use zip::ZipArchive;

use crate::config::Settings;
use crate::epub_io::{find_entry, write_epub_copy};
use crate::html::{
    append_child, create_element, create_text, find_elements, find_first_element, get_attribute,
    parse_document_html, parse_fragment_html, replace_children, serialize_document, set_attribute,
};
use crate::state::{SegmentRecord, StateStore};
use crate::tags::restore_tags;

static SLUG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\\/:*?"<>|\s]+"#).unwrap());

const BILINGUAL_CSS: &str = r#"
.bc-zh { }
.bc-src { color: #666; font-size: 0.9em; }
.bc-src i, .bc-src em { font-style: italic; }
"#;

pub struct EpubBuilder {
    settings: Arc<Settings>,
    store: Arc<StateStore>,
}

impl EpubBuilder {
    pub fn new(settings: Arc<Settings>, store: Arc<StateStore>) -> Self {
        Self { settings, store }
    }

    pub fn export_all(&self) -> Result<HashMap<String, PathBuf>> {
        let meta = self
            .store
            .load_book_meta()?
            .ok_or_else(|| anyhow::anyhow!("尚未解析书籍，请先运行 prepare"))?;
        let source = PathBuf::from(&meta.source_path);
        if !source.exists() {
            anyhow::bail!("原始 EPUB 已不在: {}", source.display());
        }

        let chapters = self.store.list_chapters()?;
        let mut segments_by_chapter = HashMap::new();
        let mut flat_segments = Vec::new();
        for chapter in &chapters {
            let segments = self.store.list_segments(Some(&chapter.id))?;
            flat_segments.extend(segments.clone());
            segments_by_chapter.insert(chapter.id.clone(), segments);
        }
        let missing = flat_segments
            .iter()
            .filter(|segment| {
                segment
                    .final_text
                    .as_deref()
                    .map(|text| text.trim().is_empty())
                    .unwrap_or(true)
            })
            .count();
        if missing > 0 {
            warn!("有 {missing} 段尚无终稿译文，导出时回退为原文");
        }

        let output_dir = self.settings.output_dir();
        std::fs::create_dir_all(&output_dir)?;
        let slug = slugify(&meta.title);

        let zh_epub = output_dir.join(format!("{slug}.zh.epub"));
        let bilingual_epub = output_dir.join(format!("{slug}.bilingual.epub"));
        let zh_md = output_dir.join(format!("{slug}.zh.md"));
        let bilingual_md = output_dir.join(format!("{slug}.bilingual.md"));
        let qa_report = output_dir.join("qa_report.md");

        self.build_epub(
            &source,
            &chapters,
            &segments_by_chapter,
            &zh_epub,
            &format!("{}（中文精译版）", meta.title),
            &meta.target_lang,
            false,
        )?;
        self.build_epub(
            &source,
            &chapters,
            &segments_by_chapter,
            &bilingual_epub,
            &format!("{}（双语对照版）", meta.title),
            &meta.target_lang,
            true,
        )?;
        self.write_markdown(&meta, &chapters, &segments_by_chapter, &zh_md, false)?;
        self.write_markdown(&meta, &chapters, &segments_by_chapter, &bilingual_md, true)?;
        self.write_qa_report(&meta, &qa_report, missing)?;

        let mut result = HashMap::new();
        result.insert("zh_epub".to_string(), zh_epub);
        result.insert("bilingual_epub".to_string(), bilingual_epub);
        result.insert("zh_md".to_string(), zh_md);
        result.insert("bilingual_md".to_string(), bilingual_md);
        result.insert("qa_report".to_string(), qa_report);
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_epub(
        &self,
        source: &Path,
        chapters: &[crate::state::ChapterRecord],
        segments_by_chapter: &HashMap<String, Vec<SegmentRecord>>,
        output: &Path,
        title: &str,
        target_lang: &str,
        bilingual: bool,
    ) -> Result<()> {
        let names = {
            let file = std::fs::File::open(source)?;
            let archive = ZipArchive::new(file)?;
            archive
                .file_names()
                .filter_map(|name| name.ok().map(|name| name.to_string()))
                .collect::<Vec<_>>()
        };

        let mut replacements = HashMap::new();
        for chapter in chapters {
            let Some(entry) = find_entry(&names, &chapter.doc_name) else {
                warn!(
                    "EPUB 中找不到文档 {}，跳过章节 {}",
                    chapter.doc_name, chapter.id
                );
                continue;
            };
            let template_path = Path::new(&chapter.template_path);
            if !template_path.exists() {
                warn!("模板缺失: {}", template_path.display());
                continue;
            }
            let template = std::fs::read_to_string(template_path)?;
            let root = parse_document_html(template.as_bytes());
            let segments: HashMap<String, SegmentRecord> = segments_by_chapter
                .get(&chapter.id)
                .map(|segments| {
                    segments
                        .iter()
                        .map(|segment| (segment.id.clone(), segment.clone()))
                        .collect()
                })
                .unwrap_or_default();
            fill_soup(&root, &segments, bilingual);
            replacements.insert(entry, serialize_document(&root).into_bytes());
        }

        write_epub_copy(
            source,
            output,
            &replacements,
            Some(title),
            Some(target_lang),
        )?;
        info!("已导出 {}", output.display());
        Ok(())
    }

    fn write_markdown(
        &self,
        meta: &crate::models::BookMeta,
        chapters: &[crate::state::ChapterRecord],
        segments_by_chapter: &HashMap<String, Vec<SegmentRecord>>,
        path: &Path,
        bilingual: bool,
    ) -> Result<()> {
        let mut lines = vec![format!("# {}", meta.title), String::new()];
        if !meta.author.is_empty() {
            lines.push(format!("*{}*", meta.author));
            lines.push(String::new());
        }
        if bilingual {
            lines.push("> 双语对照：上为中文精译，下为原文。".into());
            lines.push(String::new());
        }

        for chapter in chapters {
            lines.push(format!("## {}", chapter.title));
            lines.push(String::new());
            for segment in segments_by_chapter
                .get(&chapter.id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let translation = translated_inner(segment);
                if bilingual {
                    lines.push(translation);
                    lines.push(String::new());
                    lines.push(format!("> {}", segment.source_html));
                    lines.push(String::new());
                } else {
                    lines.push(translation);
                    lines.push(String::new());
                }
            }
        }

        std::fs::write(path, lines.join("\n").trim_end().to_string() + "\n")?;
        info!("已导出 {}", path.display());
        Ok(())
    }

    fn write_qa_report(
        &self,
        meta: &crate::models::BookMeta,
        path: &Path,
        missing: usize,
    ) -> Result<()> {
        let stats = self.store.stats()?;
        let failed = self.store.failed_segments()?;
        let now = Utc::now().to_rfc3339();

        let mut lines = vec![
            "# BookCraft-LLM QA 报告".to_string(),
            String::new(),
            format!("- 书名：{}", meta.title),
            format!("- 生成时间：{now}"),
            format!("- 段落总数：{}", stats.segments),
            format!("- 未翻译（导出回退原文）：{missing}"),
            format!(
                "- Token：{} 输入 / {} 输出",
                stats.prompt_tokens, stats.completion_tokens
            ),
            format!("- 累计成本：${:.4}", stats.cost_usd),
            String::new(),
            "## 状态分布".into(),
            String::new(),
        ];
        for (state, count) in &stats.states {
            lines.push(format!("- {state}: {count}"));
        }
        lines.push(String::new());
        lines.push("## 按阶段成本".into());
        lines.push(String::new());
        for stage in &stats.by_stage {
            lines.push(format!(
                "- {}: {} 次 / ${:.4}",
                stage.stage, stage.calls, stage.cost_usd
            ));
        }
        lines.push(String::new());
        lines.push("## 需人工复核".into());
        lines.push(String::new());
        if failed.is_empty() {
            lines.push("无。".into());
        } else {
            lines.push("| 段落 | 状态 | 分数 | 原因 |".into());
            lines.push("| --- | --- | --- | --- |".into());
            for segment in &failed {
                let reason = segment
                    .last_error
                    .clone()
                    .unwrap_or_default()
                    .replace('|', "\\|")
                    .replace('\n', " ")
                    .chars()
                    .take(300)
                    .collect::<String>();
                let score = segment
                    .qa_score
                    .map(|score| format!("{score:.1}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "| {} | {} | {} | {} |",
                    segment.id,
                    segment.state.as_str(),
                    score,
                    reason
                ));
            }
        }
        lines.push(String::new());

        std::fs::write(path, lines.join("\n"))?;
        info!("已导出 {}", path.display());
        Ok(())
    }
}

fn fill_soup(
    root: &crate::html::Handle,
    segments: &HashMap<String, SegmentRecord>,
    bilingual: bool,
) {
    if bilingual {
        if let Some(head) = find_first_element(root, "head") {
            let style = create_element("style", &[]);
            append_child(&style, create_text(BILINGUAL_CSS));
            append_child(&head, style);
        }
    }

    let elements = find_elements(root, |node| get_attribute(node, "data-bc-seg").is_some());
    for element in elements {
        let Some(segment_id) = get_attribute(&element, "data-bc-seg") else {
            continue;
        };
        let Some(segment) = segments.get(&segment_id) else {
            continue;
        };
        let zh = translated_inner(segment);
        let inner = if bilingual {
            format!(
                r#"<span class="bc-zh">{zh}</span><br/><span class="bc-src">{}</span>"#,
                segment.source_html
            )
        } else {
            zh
        };
        let nodes = parse_fragment_html(&inner);
        replace_children(&element, nodes);
        set_attribute(&element, "data-bc-seg", &segment_id);
    }
}

fn translated_inner(segment: &SegmentRecord) -> String {
    match segment.final_text.as_deref() {
        Some(text) if !text.trim().is_empty() => restore_tags(text, &segment.tag_map),
        _ => segment.source_html.clone(),
    }
}

fn slugify(text: &str) -> String {
    let slug = SLUG_RE.replace_all(text, "_");
    let slug = slug.trim_matches('_');
    let slug: String = slug.chars().take(80).collect();
    if slug.is_empty() {
        "book".to_string()
    } else {
        slug
    }
}
