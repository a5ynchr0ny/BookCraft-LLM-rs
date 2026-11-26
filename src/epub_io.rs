//! EPUB 容器级无损读写（直接操作 zip，不重建 HTML 外壳）。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use regex::Regex;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::tags::escape_html;

pub fn find_entry(names: &[String], doc_name: &str) -> Option<String> {
    let normalized = doc_name.trim_start_matches('/');
    if names.iter().any(|name| name == normalized) {
        return Some(normalized.to_string());
    }

    let suffix = format!("/{normalized}");
    let candidates: Vec<&String> = names
        .iter()
        .filter(|name| name.ends_with(&suffix))
        .collect();
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }

    let decoded_doc = percent_decode_str(normalized)
        .decode_utf8_lossy()
        .to_string();
    let decoded_suffix = format!("/{decoded_doc}");
    let decoded: Vec<&String> = names
        .iter()
        .filter(|name| {
            percent_decode_str(name)
                .decode_utf8_lossy()
                .ends_with(&decoded_suffix)
        })
        .collect();
    if decoded.len() == 1 {
        return Some(decoded[0].clone());
    }
    None
}

pub fn read_document<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    doc_name: &str,
) -> Result<Option<Vec<u8>>> {
    let names: Vec<String> = archive
        .file_names()
        .filter_map(|name| name.ok().map(|name| name.to_string()))
        .collect();
    let Some(entry) = find_entry(&names, doc_name) else {
        return Ok(None);
    };
    let mut file = archive
        .by_name(&entry)
        .with_context(|| format!("读取 EPUB 条目失败: {entry}"))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    Ok(Some(buffer))
}

pub fn write_epub_copy(
    source: &Path,
    output: &Path,
    replacements: &HashMap<String, Vec<u8>>,
    dc_title: Option<&str>,
    dc_language: Option<&str>,
) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp: PathBuf = output.with_file_name(format!(
        "{}.tmp",
        output
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("book.epub")
    ));

    let source_file =
        File::open(source).with_context(|| format!("打开原始 EPUB 失败: {}", source.display()))?;
    let mut archive = ZipArchive::new(source_file)?;
    let tmp_file = File::create(&tmp)?;
    let mut writer = ZipWriter::new(tmp_file);

    let names: Vec<String> = archive
        .file_names()
        .filter_map(|name| name.ok().map(|name| name.to_string()))
        .collect();
    let has_mimetype = names.iter().any(|name| name == "mimetype");
    if !has_mimetype {
        writer.start_file(
            "mimetype",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )?;
        writer.write_all(b"application/epub+zip")?;
    }

    for index in 0..archive.len() {
        let (name, last_modified) = {
            let entry = archive.by_index_raw(index)?;
            (entry.name()?.to_string(), entry.last_modified())
        };

        if let Some(data) = replacements.get(&name) {
            let method = if name == "mimetype" {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            let options = SimpleFileOptions::default()
                .compression_method(method)
                .last_modified_time(last_modified.unwrap_or_default());
            writer.start_file(&name, options)?;
            writer.write_all(data)?;
        } else if name == "mimetype" {
            // mimetype 在合法 EPUB 中必须是 Stored，直接按原始字节复制
            let mut data = Vec::new();
            archive.by_index_raw(index)?.read_to_end(&mut data)?;
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .last_modified_time(last_modified.unwrap_or_default());
            writer.start_file("mimetype", options)?;
            writer.write_all(&data)?;
        } else if name.to_ascii_lowercase().ends_with(".opf") {
            // 注意：by_index_raw 返回的是压缩字节，OPF 必须用 by_index 解压后再改
            let mut data = Vec::new();
            {
                let mut entry = archive.by_index(index)?;
                entry.read_to_end(&mut data)?;
            }
            let patched = patch_opf(&data, dc_title, dc_language);
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .last_modified_time(last_modified.unwrap_or_default());
            writer.start_file(&name, options)?;
            writer.write_all(&patched)?;
        } else {
            writer.raw_copy_file(archive.by_index_raw(index)?)?;
        }
    }

    writer.finish()?;
    fs::rename(&tmp, output).with_context(|| format!("写入 EPUB 失败: {}", output.display()))?;
    Ok(())
}

pub fn patch_opf(data: &[u8], title: Option<&str>, language: Option<&str>) -> Vec<u8> {
    let mut text = String::from_utf8_lossy(data).into_owned();

    if let Some(title) = title {
        let escaped = escape_html(title);
        let re = Regex::new(r"(?s)(<dc:title\b[^>]*>)(.*?)(</dc:title>)").expect("opf title regex");
        if re.is_match(&text) {
            text = re
                .replace(&text, |caps: &regex::Captures| {
                    format!("{}{}{}", &caps[1], escaped, &caps[3])
                })
                .into_owned();
        } else {
            let fallback =
                Regex::new(r"(?s)(<title\b[^>]*>)(.*?)(</title>)").expect("opf fallback");
            text = fallback
                .replace(&text, |caps: &regex::Captures| {
                    format!("{}{}{}", &caps[1], escaped, &caps[3])
                })
                .into_owned();
        }
    }

    if let Some(language) = language {
        let escaped = escape_html(language);
        let re =
            Regex::new(r"(?s)(<dc:language\b[^>]*>)(.*?)(</dc:language>)").expect("opf language");
        text = re
            .replace(&text, |caps: &regex::Captures| {
                format!("{}{}{}", &caps[1], escaped, &caps[3])
            })
            .into_owned();
    }

    text.into_bytes()
}

/// 把 OPF/NCX/正文里的相对链接解析为 zip 内部路径。
pub fn resolve_href(base_dir: &str, href: &str) -> String {
    let decoded = percent_decode_str(href)
        .decode_utf8_lossy()
        .replace('\\', "/");
    let path = decoded.split('#').next().unwrap_or("");
    if path.is_empty() {
        return String::new();
    }
    let combined = if path.starts_with('/') {
        path.trim_start_matches('/').to_string()
    } else if base_dir.is_empty() {
        path.to_string()
    } else {
        format!("{base_dir}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in combined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}
