//! 行内标签占位符协议：`[tag_1]...[/tag_1]` / `[tag_2/]`。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

pub static TAG_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(/?)(tag_\d+)(/?)\]").expect("tag regex"));

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TagEntry {
    pub open: String,
    pub close: String,
}

pub type TagMap = HashMap<String, TagEntry>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkerKind {
    Open,
    Close,
    Void,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub kind: MarkerKind,
    pub tag_id: String,
    pub start: usize,
    pub end: usize,
}

pub fn parse_markers(text: &str) -> Vec<Marker> {
    let mut markers = Vec::new();
    for captures in TAG_MARKER_RE.captures_iter(text) {
        let whole = captures.get(0).expect("whole match");
        let is_close = !captures[1].is_empty();
        let is_void = !captures[3].is_empty();
        if is_close && is_void {
            continue;
        }
        let kind = if is_close {
            MarkerKind::Close
        } else if is_void {
            MarkerKind::Void
        } else {
            MarkerKind::Open
        };
        markers.push(Marker {
            kind,
            tag_id: captures[2].to_string(),
            start: whole.start(),
            end: whole.end(),
        });
    }
    markers
}

pub fn tag_counts(text: &str) -> HashMap<(String, MarkerKind), usize> {
    let mut counts = HashMap::new();
    for marker in parse_markers(text) {
        *counts.entry((marker.tag_id, marker.kind)).or_insert(0) += 1;
    }
    counts
}

/// 硬校验：占位符数量、未知 ID、闭合顺序。
pub fn validate_tags(source: &str, translation: &str, tag_map: &TagMap) -> Vec<String> {
    let mut errors = Vec::new();
    let src_counts = tag_counts(source);
    let dst_counts = tag_counts(translation);

    for tag_id in tag_map.keys() {
        for kind in [MarkerKind::Open, MarkerKind::Close, MarkerKind::Void] {
            let expected = src_counts
                .get(&(tag_id.clone(), kind))
                .copied()
                .unwrap_or(0);
            let actual = dst_counts
                .get(&(tag_id.clone(), kind))
                .copied()
                .unwrap_or(0);
            if expected != actual {
                errors.push(format!(
                    "标签 {tag_id} 的 {} 数量不一致：原文 {expected} / 译文 {actual}",
                    kind_name(kind)
                ));
            }
        }
    }

    for marker in parse_markers(translation) {
        if !tag_map.contains_key(&marker.tag_id) {
            errors.push(format!("译文中出现未定义的占位符 {}", marker.tag_id));
        }
    }

    // 校验译文中的嵌套顺序
    let mut stack: Vec<String> = Vec::new();
    for marker in parse_markers(translation) {
        match marker.kind {
            MarkerKind::Open => stack.push(marker.tag_id),
            MarkerKind::Void => {}
            MarkerKind::Close => {
                if stack.last() != Some(&marker.tag_id) {
                    errors.push(format!(
                        "标签闭合顺序错误：[/{}] 与当前栈 {:?} 不匹配",
                        marker.tag_id,
                        stack.iter().rev().take(3).cloned().collect::<Vec<_>>()
                    ));
                    if let Some(position) = stack.iter().position(|item| item == &marker.tag_id) {
                        stack.truncate(position);
                    }
                    continue;
                }
                stack.pop();
            }
        }
    }
    if !stack.is_empty() {
        errors.push(format!("存在未闭合标签：{stack:?}"));
    }

    // 原文自身也必须成对，否则说明解析器有 bug
    let mut src_stack: Vec<String> = Vec::new();
    for marker in parse_markers(source) {
        match marker.kind {
            MarkerKind::Open => src_stack.push(marker.tag_id),
            MarkerKind::Void => {}
            MarkerKind::Close => {
                if src_stack.last() == Some(&marker.tag_id) {
                    src_stack.pop();
                }
            }
        }
    }
    if !src_stack.is_empty() {
        errors.push(format!("原文占位符未闭合（内部错误）：{src_stack:?}"));
    }

    errors
}

fn kind_name(kind: MarkerKind) -> &'static str {
    match kind {
        MarkerKind::Open => "open",
        MarkerKind::Close => "close",
        MarkerKind::Void => "void",
    }
}

/// 把占位符还原为 HTML；文本部分统一转义。
pub fn restore_tags(text: &str, tag_map: &TagMap) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut cursor = 0;
    for marker in parse_markers(text) {
        out.push_str(&escape_html(&text[cursor..marker.start]));
        match tag_map.get(&marker.tag_id) {
            None => out.push_str(&escape_html(&text[marker.start..marker.end])),
            Some(entry) => match marker.kind {
                MarkerKind::Close => out.push_str(&entry.close),
                MarkerKind::Void | MarkerKind::Open => out.push_str(&entry.open),
            },
        }
        cursor = marker.end;
    }
    out.push_str(&escape_html(&text[cursor..]));
    out
}

pub fn strip_tags(text: &str) -> String {
    TAG_MARKER_RE.replace_all(text, "").into_owned()
}

pub fn has_markers(text: &str) -> bool {
    TAG_MARKER_RE.is_match(text)
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}
