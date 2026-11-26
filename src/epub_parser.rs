//! EPUB 解析：按自然段切分、行内标签占位符化、保留可回填的 DOM 骨架。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use markup5ever_rcdom::{Handle, NodeData};
use percent_encoding::percent_decode_str;
use regex::Regex;
use roxmltree::Document as XmlDocument;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::epub_io::{read_document, resolve_href};
use crate::html::{
    children, element_name, find_elements, inner_html, is_block_tag, is_skip_tag, is_void_tag,
    normalize_whitespace, opening_tag, parse_document_html, remove_children, serialize_document,
    set_attribute, text_content,
};
use crate::models::BookMeta;
use crate::state::{ChapterRecord, SegmentRecord};
use crate::tags::{TagEntry, TagMap, strip_tags};

static CJK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\u{3040}-\u{30ff}\u{3400}-\u{9fff}\u{ac00}-\u{d7af}]").unwrap());
static WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{L}+(?:['’\-]\p{L}+)*").unwrap());

#[derive(Debug, Clone, Default)]
pub struct ParseResult {
    pub meta: BookMeta,
    pub chapters: Vec<ChapterRecord>,
    pub segments: Vec<SegmentRecord>,
}

#[derive(Debug, Clone)]
struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
    properties: String,
    zip_path: String,
}

pub struct EpubParser {
    epub_path: PathBuf,
    templates_dir: PathBuf,
    title_override: String,
    author_override: String,
    source_lang: String,
    target_lang: String,
}

impl EpubParser {
    pub fn new(
        epub_path: impl AsRef<Path>,
        templates_dir: impl AsRef<Path>,
        title: &str,
        author: &str,
        source_lang: &str,
        target_lang: &str,
    ) -> Self {
        Self {
            epub_path: epub_path.as_ref().to_path_buf(),
            templates_dir: templates_dir.as_ref().to_path_buf(),
            title_override: title.to_string(),
            author_override: author.to_string(),
            source_lang: source_lang.to_string(),
            target_lang: target_lang.to_string(),
        }
    }

    pub fn parse(&self) -> Result<ParseResult> {
        if !self.epub_path.exists() {
            anyhow::bail!("EPUB 不存在: {}", self.epub_path.display());
        }
        std::fs::create_dir_all(&self.templates_dir)?;

        let file = std::fs::File::open(&self.epub_path)
            .with_context(|| format!("打开 EPUB 失败: {}", self.epub_path.display()))?;
        let mut archive = ZipArchive::new(file)?;

        let container = read_document(&mut archive, "META-INF/container.xml")?
            .ok_or_else(|| anyhow::anyhow!("EPUB 缺少 META-INF/container.xml"))?;
        let container_text = String::from_utf8_lossy(&container);
        let container_doc =
            XmlDocument::parse(&container_text).context("解析 container.xml 失败")?;
        let opf_path = container_doc
            .descendants()
            .find(|node| node.has_tag_name("rootfile"))
            .and_then(|node| node.attribute("full-path"))
            .map(|value| percent_decode_str(value).decode_utf8_lossy().to_string())
            .ok_or_else(|| anyhow::anyhow!("container.xml 中没有 rootfile"))?;

        let opf_bytes = read_document(&mut archive, &opf_path)?
            .ok_or_else(|| anyhow::anyhow!("EPUB 缺少 OPF: {opf_path}"))?;
        let opf_text = String::from_utf8_lossy(&opf_bytes);
        let opf = XmlDocument::parse(&opf_text).context("解析 OPF 失败")?;
        let opf_dir = Path::new(&opf_path)
            .parent()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();

        let meta = self.build_meta(&opf)?;
        let manifest = parse_manifest(&opf, &opf_dir);
        let spine = parse_spine(&opf);

        let toc_titles = self.parse_toc_titles(&mut archive, &manifest, &opf_dir)?;

        let mut ordered: Vec<&ManifestItem> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for idref in &spine {
            if let Some(item) = manifest.iter().find(|item| &item.id == idref) {
                if is_nav_item(item) || !is_html_item(item) {
                    continue;
                }
                if seen.insert(item.zip_path.clone()) {
                    ordered.push(item);
                }
            }
        }
        for item in &manifest {
            if is_nav_item(item) || !is_html_item(item) {
                continue;
            }
            if seen.insert(item.zip_path.clone()) {
                ordered.push(item);
            }
        }

        let mut chapters = Vec::new();
        let mut segments = Vec::new();
        for (doc_index, item) in ordered.iter().enumerate() {
            let chapter_number = doc_index + 1;
            let chapter_id = format!("ch_{chapter_number:03}");
            let Some(bytes) = read_document(&mut archive, &item.zip_path)? else {
                continue;
            };
            let root = parse_document_html(&bytes);
            let chapter_segments = extract_segments(&root, &chapter_id);
            if chapter_segments.is_empty() {
                continue;
            }

            let title = toc_titles
                .get(&item.zip_path)
                .cloned()
                .or_else(|| {
                    Path::new(&item.zip_path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| toc_titles.get(name).cloned())
                })
                .or_else(|| first_heading_text(&root))
                .unwrap_or_else(|| format!("第 {chapter_number} 章"));

            let template_path = self.templates_dir.join(format!("{chapter_id}.xhtml"));
            let template = serialize_document(&root);
            std::fs::write(&template_path, template)?;

            chapters.push(ChapterRecord {
                id: chapter_id.clone(),
                order_index: chapters.len() as i64 + 1,
                doc_name: item.zip_path.clone(),
                title,
                template_path: template_path.to_string_lossy().to_string(),
                status: "PENDING".into(),
            });
            segments.extend(chapter_segments);
        }

        if chapters.is_empty() {
            anyhow::bail!("EPUB 中没有找到可翻译的正文段落");
        }

        let mut meta = meta;
        meta.chapter_count = chapters.len() as i64;
        meta.segment_count = segments.len() as i64;
        Ok(ParseResult {
            meta,
            chapters,
            segments,
        })
    }

    fn build_meta(&self, opf: &XmlDocument<'_>) -> Result<BookMeta> {
        let metadata_node = opf.descendants().find(|node| node.has_tag_name("metadata"));
        let title = if self.title_override.is_empty() {
            metadata_node
                .and_then(|node| child_text(&node, "title"))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    Path::new(&self.epub_path)
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("book")
                        .to_string()
                })
        } else {
            self.title_override.clone()
        };
        let author = if self.author_override.is_empty() {
            metadata_node
                .and_then(|node| child_text(&node, "creator"))
                .unwrap_or_default()
        } else {
            self.author_override.clone()
        };
        let language = metadata_node
            .and_then(|node| child_text(&node, "language"))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.source_lang.clone());

        let bytes = std::fs::read(&self.epub_path)?;
        let digest = Sha256::digest(&bytes);
        let sha = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let canonical =
            std::fs::canonicalize(&self.epub_path).unwrap_or_else(|_| self.epub_path.clone());

        Ok(BookMeta {
            title,
            author,
            source_lang: language,
            target_lang: self.target_lang.clone(),
            source_path: canonical.to_string_lossy().to_string(),
            source_sha256: sha,
            chapter_count: 0,
            segment_count: 0,
        })
    }

    fn parse_toc_titles(
        &self,
        archive: &mut ZipArchive<std::fs::File>,
        manifest: &[ManifestItem],
        opf_dir: &str,
    ) -> Result<HashMap<String, String>> {
        let mut titles = HashMap::new();
        let ncx = manifest.iter().find(|item| {
            item.media_type
                .eq_ignore_ascii_case("application/x-dtbncx+xml")
                || item.href.to_ascii_lowercase().ends_with("toc.ncx")
        });
        let Some(ncx) = ncx else {
            return Ok(titles);
        };
        let Some(bytes) = read_document(archive, &ncx.zip_path)? else {
            return Ok(titles);
        };
        let text = String::from_utf8_lossy(&bytes);
        let Ok(doc) = XmlDocument::parse(&text) else {
            return Ok(titles);
        };
        let ncx_dir = Path::new(&ncx.zip_path)
            .parent()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| opf_dir.to_string());

        for nav_point in doc
            .descendants()
            .filter(|node| node.has_tag_name("navPoint"))
        {
            let label = nav_point
                .children()
                .find(|node| node.has_tag_name("navLabel"))
                .and_then(|node| node.children().find(|child| child.has_tag_name("text")))
                .map(|node| node_text(&node))
                .unwrap_or_default();
            let src = nav_point
                .children()
                .find(|node| node.has_tag_name("content"))
                .and_then(|node| node.attribute("src"))
                .unwrap_or("");
            if label.is_empty() || src.is_empty() {
                continue;
            }
            let resolved = resolve_href(&ncx_dir, src);
            if resolved.is_empty() {
                continue;
            }
            titles
                .entry(resolved.clone())
                .or_insert_with(|| label.clone());
            if let Some(name) = Path::new(&resolved)
                .file_name()
                .and_then(|name| name.to_str())
            {
                titles.entry(name.to_string()).or_insert(label);
            }
        }
        Ok(titles)
    }
}

fn parse_manifest(opf: &XmlDocument<'_>, opf_dir: &str) -> Vec<ManifestItem> {
    let mut items = Vec::new();
    let Some(manifest) = opf.descendants().find(|node| node.has_tag_name("manifest")) else {
        return items;
    };
    for item in manifest.children().filter(|node| node.has_tag_name("item")) {
        let id = item.attribute("id").unwrap_or("").to_string();
        let href = item.attribute("href").unwrap_or("");
        if id.is_empty() || href.is_empty() {
            continue;
        }
        let zip_path = resolve_href(opf_dir, href);
        if zip_path.is_empty() {
            continue;
        }
        items.push(ManifestItem {
            id,
            href: href.to_string(),
            media_type: item.attribute("media-type").unwrap_or("").to_string(),
            properties: item.attribute("properties").unwrap_or("").to_string(),
            zip_path,
        });
    }
    items
}

fn parse_spine(opf: &XmlDocument<'_>) -> Vec<String> {
    let Some(spine) = opf.descendants().find(|node| node.has_tag_name("spine")) else {
        return Vec::new();
    };
    spine
        .children()
        .filter(|node| node.has_tag_name("itemref"))
        .filter_map(|node| node.attribute("idref").map(|value| value.to_string()))
        .collect()
}

fn is_html_item(item: &ManifestItem) -> bool {
    let media = item.media_type.to_ascii_lowercase();
    let href = item.href.to_ascii_lowercase();
    media.contains("html")
        || href.ends_with(".xhtml")
        || href.ends_with(".html")
        || href.ends_with(".htm")
}

fn is_nav_item(item: &ManifestItem) -> bool {
    item.properties
        .split_whitespace()
        .any(|property| property == "nav")
        || item.href.to_ascii_lowercase().ends_with("nav.xhtml")
}

fn child_text(node: &roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|child| child.has_tag_name(name))
        .map(|child| node_text(&child))
}

fn node_text(node: &roxmltree::Node<'_, '_>) -> String {
    node.descendants()
        .filter(|child| child.is_text())
        .filter_map(|child| child.text())
        .collect::<String>()
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// 段落抽取
// ---------------------------------------------------------------------------

fn extract_segments(root: &Handle, chapter_id: &str) -> Vec<SegmentRecord> {
    let mut blocks = Vec::new();
    collect_leaf_blocks(root, &mut blocks);

    let mut segments = Vec::new();
    for block in blocks {
        let source_html = inner_html(&block);
        let extraction = extract_inline(&block);
        let source_text = extraction.text.trim().to_string();
        if !has_letters(&source_text) {
            continue;
        }
        let index = segments.len() + 1;
        let segment_id = format!("{chapter_id}_s{index:04}");
        set_attribute(&block, "data-bc-seg", &segment_id);
        remove_children(&block);

        segments.push(SegmentRecord {
            id: segment_id,
            chapter_id: chapter_id.to_string(),
            order_index: index as i64,
            source_words: count_source_words(&source_text) as i64,
            is_dialogue: is_dialogue(&source_text),
            source_text,
            source_html,
            tag_map: extraction.tag_map,
            state: crate::models::SegmentState::Pending,
            pass1_notes: None,
            pass1_draft: None,
            final_text: None,
            best_text: None,
            qa_score: None,
            last_routing: None,
            last_error: None,
            retry_count: 0,
            updated_at: String::new(),
        });
    }
    segments
}

fn collect_leaf_blocks(node: &Handle, out: &mut Vec<Handle>) {
    if let Some(name) = element_name(node) {
        if is_skip_tag(&name) {
            return;
        }
        if is_block_tag(&name) && !has_block_descendant(node) {
            out.push(node.clone());
        }
    }
    for child in children(node) {
        collect_leaf_blocks(&child, out);
    }
}

fn has_block_descendant(node: &Handle) -> bool {
    for child in children(node) {
        if let Some(name) = element_name(&child) {
            if is_skip_tag(&name) {
                continue;
            }
            if is_block_tag(&name) {
                return true;
            }
        }
        if has_block_descendant(&child) {
            return true;
        }
    }
    false
}

struct InlineExtraction {
    text: String,
    tag_map: TagMap,
}

fn extract_inline(block: &Handle) -> InlineExtraction {
    let mut state = InlineState {
        text: String::new(),
        tag_map: HashMap::new(),
        counter: 0,
    };
    walk_inline(block, &mut state);
    InlineExtraction {
        text: state.text,
        tag_map: state.tag_map,
    }
}

struct InlineState {
    text: String,
    tag_map: TagMap,
    counter: usize,
}

fn walk_inline(node: &Handle, state: &mut InlineState) {
    for child in children(node) {
        match &child.data {
            NodeData::Text { contents } => {
                state
                    .text
                    .push_str(&normalize_whitespace(&contents.borrow()));
            }
            NodeData::Element { name, .. } => {
                let tag_name = name.local.to_string();
                if tag_name == "script" || tag_name == "style" {
                    continue;
                }
                state.counter += 1;
                let tag_id = format!("tag_{}", state.counter);
                if is_void_tag(&tag_name) {
                    state.tag_map.insert(
                        tag_id.clone(),
                        TagEntry {
                            open: opening_tag(&child, true),
                            close: String::new(),
                        },
                    );
                    state.text.push_str(&format!("[{tag_id}/]"));
                } else {
                    state.tag_map.insert(
                        tag_id.clone(),
                        TagEntry {
                            open: opening_tag(&child, false),
                            close: format!("</{tag_name}>"),
                        },
                    );
                    state.text.push_str(&format!("[{tag_id}]"));
                    walk_inline(&child, state);
                    state.text.push_str(&format!("[/{tag_id}]"));
                }
            }
            _ => {}
        }
    }
}

fn first_heading_text(root: &Handle) -> Option<String> {
    let headings = find_elements(root, |node| {
        matches!(
            element_name(node).as_deref(),
            Some("h1") | Some("h2") | Some("h3")
        )
    });
    headings
        .first()
        .map(|node| text_content(node).trim().to_string())
        .filter(|text| !text.is_empty())
}

fn has_letters(text: &str) -> bool {
    strip_tags(text).chars().any(|ch| ch.is_alphabetic())
}

fn is_dialogue(text: &str) -> bool {
    let text = strip_tags(text);
    text.contains('«')
        || text.contains('»')
        || text.contains('“')
        || text.contains('”')
        || text.contains('"')
}

pub fn count_source_words(text: &str) -> usize {
    let clean = strip_tags(text);
    if CJK_RE.is_match(&clean) {
        CJK_RE.find_iter(&clean).count()
    } else {
        WORD_RE.find_iter(&clean).count()
    }
}
