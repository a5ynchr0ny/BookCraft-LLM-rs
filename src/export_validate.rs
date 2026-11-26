//! 导出硬校验：段落占位符与 EPUB 结构/链接完整性。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use roxmltree::{Document, ParsingOptions};
use zip::{CompressionMethod, ZipArchive};

use crate::epub_io::resolve_href;
use crate::state::SegmentRecord;
use crate::tags::validate_tags;

static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:href|src)="([^"]+)""#).unwrap());

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ExportValidationError(pub String);

/// 段落级硬校验：缺译、未知占位符、标签数量/嵌套。
pub fn validate_segments(segments: &[SegmentRecord]) -> Vec<String> {
    let mut problems = Vec::new();
    for segment in segments {
        let text = segment.final_text.as_deref().unwrap_or("").trim();
        if text.is_empty() {
            problems.push(format!("{}: 没有终稿译文", segment.id));
            continue;
        }
        for error in validate_tags(&segment.source_text, text, &segment.tag_map) {
            problems.push(format!("{}: {error}", segment.id));
        }
    }
    problems
}

/// EPUB 结构硬校验：mimetype、manifest、spine、NCX、相对链接、残留占位符。
pub fn validate_epub(path: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let Ok(file) = File::open(path) else {
        return vec![format!("无法打开: {}", path.display())];
    };
    let Ok(mut archive) = ZipArchive::new(file) else {
        return vec!["不是合法 zip".to_string()];
    };
    let names: Vec<String> = archive
        .file_names()
        .filter_map(|name| name.ok().map(|name| name.to_string()))
        .collect();
    let name_set: HashSet<&str> = names.iter().map(String::as_str).collect();

    if names.first().map(String::as_str) != Some("mimetype") {
        problems.push("mimetype 不是第一个条目".to_string());
    } else {
        if let Ok(entry) = archive.by_index_raw(0) {
            if entry.compression() != CompressionMethod::Stored {
                problems.push("mimetype 被压缩（应为 stored）".to_string());
            }
        }
        match read_entry(&mut archive, "mimetype") {
            Some(data) if String::from_utf8_lossy(&data).trim() == "application/epub+zip" => {}
            Some(_) => problems.push("mimetype 内容不正确".to_string()),
            None => problems.push("无法读取 mimetype".to_string()),
        }
    }

    // zip 数据完整性：逐个解压到 sink
    for index in 0..archive.len() {
        match archive.by_index(index) {
            Ok(mut entry) => {
                let mut sink = std::io::sink();
                if let Err(error) = std::io::copy(&mut entry, &mut sink) {
                    let name = entry
                        .name()
                        .map(|name| name.to_string())
                        .unwrap_or_default();
                    problems.push(format!("zip 数据损坏: {name}: {error}"));
                }
            }
            Err(error) => problems.push(format!("zip 条目无法读取: {error}")),
        }
    }

    if !name_set.contains("META-INF/container.xml") {
        problems.push("缺少 META-INF/container.xml".to_string());
        return problems;
    }
    let Some(container_bytes) = read_entry(&mut archive, "META-INF/container.xml") else {
        problems.push("无法读取 META-INF/container.xml".to_string());
        return problems;
    };
    let container_text = String::from_utf8_lossy(&container_bytes);
    let container = match parse_xml(&container_text) {
        Ok(document) => document,
        Err(error) => {
            problems.push(format!("container.xml 解析失败: {error}"));
            return problems;
        }
    };
    let opf_path = container
        .descendants()
        .find(|node| node.has_tag_name("rootfile"))
        .and_then(|node| node.attribute("full-path"))
        .unwrap_or("");
    if opf_path.is_empty() || !name_set.contains(opf_path) {
        problems.push(format!("OPF 不存在: {opf_path}"));
        return problems;
    }

    let Some(opf_bytes) = read_entry(&mut archive, opf_path) else {
        problems.push(format!("无法读取 OPF: {opf_path}"));
        return problems;
    };
    let opf_text = String::from_utf8_lossy(&opf_bytes);
    let opf = match parse_xml(&opf_text) {
        Ok(document) => document,
        Err(error) => {
            problems.push(format!("OPF 解析失败: {error}"));
            return problems;
        }
    };
    let opf_dir = Path::new(opf_path)
        .parent()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();

    let mut manifest: HashMap<String, String> = HashMap::new();
    if let Some(manifest_node) = opf.descendants().find(|node| node.has_tag_name("manifest")) {
        for item in manifest_node
            .children()
            .filter(|node| node.has_tag_name("item"))
        {
            let id = item.attribute("id").unwrap_or("");
            let href = item.attribute("href").unwrap_or("");
            if id.is_empty() || href.is_empty() {
                problems.push("manifest 存在缺少 id/href 的条目".to_string());
                continue;
            }
            manifest.insert(id.to_string(), href.to_string());
            let target = resolve_href(&opf_dir, href);
            if !target.is_empty() && !name_set.contains(target.as_str()) {
                problems.push(format!("manifest 文件不存在: {href}"));
            }
        }
    }

    let mut spine_ids = Vec::new();
    if let Some(spine_node) = opf.descendants().find(|node| node.has_tag_name("spine")) {
        for itemref in spine_node
            .children()
            .filter(|node| node.has_tag_name("itemref"))
        {
            if let Some(idref) = itemref.attribute("idref") {
                spine_ids.push(idref.to_string());
            }
        }
    }
    if spine_ids.is_empty() {
        problems.push("spine 为空".to_string());
    }
    for idref in spine_ids {
        if !manifest.contains_key(&idref) {
            problems.push(format!("spine 引用了不存在的 id: {idref}"));
        }
    }

    if let Some(ncx_href) = manifest
        .values()
        .find(|href| href.to_ascii_lowercase().ends_with(".ncx"))
    {
        let ncx_path = resolve_href(&opf_dir, ncx_href);
        if name_set.contains(ncx_path.as_str()) {
            if let Some(ncx_bytes) = read_entry(&mut archive, &ncx_path) {
                let ncx_text = String::from_utf8_lossy(&ncx_bytes);
                match parse_xml(&ncx_text) {
                    Ok(ncx) => {
                        let ncx_dir = Path::new(&ncx_path)
                            .parent()
                            .map(|path| path.to_string_lossy().replace('\\', "/"))
                            .unwrap_or_default();
                        for content in ncx
                            .descendants()
                            .filter(|node| node.has_tag_name("content"))
                        {
                            let src = content.attribute("src").unwrap_or("");
                            if src.is_empty() {
                                continue;
                            }
                            let target = resolve_href(&ncx_dir, src);
                            if !target.is_empty() && !name_set.contains(target.as_str()) {
                                problems.push(format!("目录目标不存在: {src}"));
                            }
                        }
                    }
                    Err(error) => problems.push(format!("NCX 解析失败: {error}")),
                }
            }
        }
    }

    let documents: Vec<&String> = names
        .iter()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".html") || lower.ends_with(".xhtml") || lower.ends_with(".htm")
        })
        .collect();
    for name in documents {
        let Some(bytes) = read_entry(&mut archive, name) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if text.contains("[tag_") {
            problems.push(format!("正文残留占位符: {name}"));
        }
        let base = Path::new(name)
            .parent()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        for captures in LINK_RE.captures_iter(&text) {
            let Some(attr) = captures.get(1) else {
                continue;
            };
            let attr = attr.as_str();
            if attr.starts_with("http:")
                || attr.starts_with("https:")
                || attr.starts_with("mailto:")
                || attr.starts_with('#')
                || attr.starts_with("data:")
            {
                continue;
            }
            let target = resolve_href(&base, attr);
            if target.is_empty() {
                continue;
            }
            if !name_set.contains(target.as_str()) {
                problems.push(format!("相对链接失效: {name} -> {attr}"));
            }
        }
    }

    problems
}

fn read_entry<R: Read + Seek>(archive: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let mut entry = archive.by_name(name).ok()?;
    let mut data = Vec::new();
    entry.read_to_end(&mut data).ok()?;
    Some(data)
}

fn parse_xml(text: &str) -> Result<Document<'_>, roxmltree::Error> {
    Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
}
