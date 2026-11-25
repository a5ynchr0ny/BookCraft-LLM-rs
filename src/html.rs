//! HTML/XHTML DOM 工具。
//!
//! 使用 html5ever 的 RcDom 做解析与可变 DOM，并配一个自定义 XML 序列化器：
//! html5ever 自带的 serializer 输出 HTML（空元素不带 `/`），会导致重新打包后的
//! XHTML 不是合法 XML。这里统一输出 XHTML 兼容的标签，保证 EPUB 结构完好。

use std::cell::RefCell;
use std::rc::Rc;

use html5ever::tendril::TendrilSink;
use html5ever::{Attribute, LocalName, QualName, ns, parse_document};
pub use markup5ever_rcdom::Handle;
use markup5ever_rcdom::{Node, NodeData, RcDom};

use crate::tags::escape_html;

pub const BLOCK_TAGS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "blockquote",
    "div",
    "td",
    "th",
    "figcaption",
    "dd",
    "dt",
    "section",
    "article",
    "header",
    "footer",
    "aside",
    "caption",
    "address",
];

pub const SKIP_TAGS: &[&str] = &[
    "pre", "script", "style", "noscript", "svg", "math", "textarea",
];

pub const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

pub fn is_block_tag(name: &str) -> bool {
    BLOCK_TAGS.contains(&name)
}

pub fn is_skip_tag(name: &str) -> bool {
    SKIP_TAGS.contains(&name)
}

pub fn is_void_tag(name: &str) -> bool {
    VOID_TAGS.contains(&name)
}

pub fn parse_document_html(input: &[u8]) -> Handle {
    let text = String::from_utf8_lossy(input);
    let cleaned = strip_xml_prolog(&text);
    let dom = parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .one(cleaned.as_bytes());
    dom.document
}

pub fn parse_fragment_html(fragment: &str) -> Vec<Handle> {
    let wrapped = format!("<!DOCTYPE html><html><body>{fragment}</body></html>");
    let root = parse_document_html(wrapped.as_bytes());
    let Some(body) = find_first_element(&root, "body") else {
        return Vec::new();
    };
    // RcDom 的 Node::drop 会递归拆解子树。必须先把节点从 fragment 文档中摘出来，
    // 否则 root 一旦 drop，返回的节点内容也会被清空。
    let nodes: Vec<Handle> = body.children.borrow_mut().drain(..).collect();
    for node in &nodes {
        node.parent.set(None);
    }
    nodes
}

fn strip_xml_prolog(text: &str) -> String {
    let text = text.trim_start_matches('\u{feff}');
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<?xml") {
        if let Some(end) = rest.find("?>") {
            return rest[end + 2..].trim_start().to_string();
        }
    }
    text.to_string()
}

pub fn element_name(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.to_string()),
        _ => None,
    }
}

pub fn is_element(node: &Handle, name: &str) -> bool {
    element_name(node).as_deref() == Some(name)
}

pub fn children(node: &Handle) -> Vec<Handle> {
    node.children.borrow().clone()
}

pub fn walk(node: &Handle, f: &mut impl FnMut(&Handle)) {
    f(node);
    let kids = children(node);
    for child in kids {
        walk(&child, f);
    }
}

pub fn find_elements(root: &Handle, mut predicate: impl FnMut(&Handle) -> bool) -> Vec<Handle> {
    let mut found = Vec::new();
    walk(root, &mut |node| {
        if predicate(node) {
            found.push(node.clone());
        }
    });
    found
}

pub fn find_first_element(root: &Handle, name: &str) -> Option<Handle> {
    if is_element(root, name) {
        return Some(root.clone());
    }
    let kids = children(root);
    for child in kids {
        if let Some(found) = find_first_element(&child, name) {
            return Some(found);
        }
    }
    None
}

pub fn text_content(node: &Handle) -> String {
    let mut out = String::new();
    walk(node, &mut |item| {
        if let NodeData::Text { contents } = &item.data {
            out.push_str(&contents.borrow());
        }
    });
    out
}

pub fn get_attribute(node: &Handle, name: &str) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &node.data {
        for attr in attrs.borrow().iter() {
            if attr.name.local.to_string().eq_ignore_ascii_case(name) {
                return Some(attr.value.to_string());
            }
        }
    }
    None
}

pub fn set_attribute(node: &Handle, name: &str, value: &str) {
    if let NodeData::Element { attrs, .. } = &node.data {
        let mut attrs = attrs.borrow_mut();
        for attr in attrs.iter_mut() {
            if attr.name.local.to_string().eq_ignore_ascii_case(name) {
                attr.value = value.into();
                return;
            }
        }
        attrs.push(Attribute {
            name: QualName::new(None, ns!(), LocalName::from(name)),
            value: value.into(),
        });
    }
}

pub fn remove_attribute(node: &Handle, name: &str) {
    if let NodeData::Element { attrs, .. } = &node.data {
        attrs
            .borrow_mut()
            .retain(|attr| !attr.name.local.to_string().eq_ignore_ascii_case(name));
    }
}

pub fn create_element(name: &str, attrs: &[(&str, &str)]) -> Handle {
    let attributes = attrs
        .iter()
        .map(|(key, value)| Attribute {
            name: QualName::new(None, ns!(), LocalName::from(*key)),
            value: (*value).into(),
        })
        .collect();
    Node::new(NodeData::Element {
        name: QualName::new(None, ns!(html), LocalName::from(name)),
        attrs: RefCell::new(attributes),
        template_contents: RefCell::new(None),
        mathml_annotation_xml_integration_point: false,
    })
}

pub fn create_text(text: &str) -> Handle {
    Node::new(NodeData::Text {
        contents: RefCell::new(text.into()),
    })
}

pub fn append_child(parent: &Handle, child: Handle) {
    child.parent.set(Some(Rc::downgrade(parent)));
    parent.children.borrow_mut().push(child);
}

pub fn remove_children(node: &Handle) {
    node.children.borrow_mut().clear();
}

pub fn replace_children(node: &Handle, new_children: Vec<Handle>) {
    let mut kids = node.children.borrow_mut();
    kids.clear();
    for child in new_children {
        child.parent.set(Some(Rc::downgrade(node)));
        kids.push(child);
    }
}

pub fn inner_html(node: &Handle) -> String {
    let parent_name = element_name(node);
    serialize_children(node, parent_name.as_deref())
}

pub fn serialize_nodes(nodes: &[Handle]) -> String {
    let mut out = String::new();
    for node in nodes {
        serialize_node(node, None, &mut out);
    }
    out
}

pub fn serialize_document(root: &Handle) -> String {
    ensure_xhtml_root(root);
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    let has_doctype = children(root)
        .iter()
        .any(|child| matches!(child.data, NodeData::Doctype { .. }));
    if !has_doctype {
        out.push_str("<!DOCTYPE html>\n");
    }
    out.push_str(&serialize_children(root, None));
    out
}

fn ensure_xhtml_root(root: &Handle) {
    let Some(html) = find_first_element(root, "html") else {
        return;
    };
    if get_attribute(&html, "xmlns").is_none() {
        set_attribute(&html, "xmlns", "http://www.w3.org/1999/xhtml");
    }
    if get_attribute(&html, "xmlns:epub").is_none() {
        set_attribute(&html, "xmlns:epub", "http://www.idpf.org/2007/ops");
    }
}

fn serialize_children(node: &Handle, parent_name: Option<&str>) -> String {
    let mut out = String::new();
    for child in children(node) {
        serialize_node(&child, parent_name, &mut out);
    }
    out
}

fn serialize_node(node: &Handle, parent_name: Option<&str>, out: &mut String) {
    match &node.data {
        NodeData::Document => out.push_str(&serialize_children(node, parent_name)),
        NodeData::Doctype {
            name,
            public_id,
            system_id,
        } => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            if !public_id.is_empty() {
                out.push_str(" PUBLIC \"");
                out.push_str(public_id);
                out.push('"');
                if !system_id.is_empty() {
                    out.push_str(" \"");
                    out.push_str(system_id);
                    out.push('"');
                }
            } else if !system_id.is_empty() {
                out.push_str(" SYSTEM \"");
                out.push_str(system_id);
                out.push('"');
            }
            out.push_str(">\n");
        }
        NodeData::Text { contents } => {
            let raw = parent_name
                .map(|name| {
                    name.eq_ignore_ascii_case("style") || name.eq_ignore_ascii_case("script")
                })
                .unwrap_or(false);
            let text = contents.borrow();
            if raw {
                out.push_str(&text);
            } else {
                out.push_str(&escape_html(&text));
            }
        }
        NodeData::Comment { contents } => {
            out.push_str("<!--");
            out.push_str(contents);
            out.push_str("-->");
        }
        NodeData::ProcessingInstruction { target, contents } => {
            out.push_str("<?");
            out.push_str(target);
            if !contents.is_empty() {
                out.push(' ');
                out.push_str(contents);
            }
            out.push_str("?>");
        }
        NodeData::Element { name, .. } => {
            let tag = qualified_name(name);
            let void = is_void_tag(name.local.as_ref());
            out.push('<');
            out.push_str(&tag);
            out.push_str(&serialize_attributes(node, name));
            if void {
                out.push_str("/>");
                return;
            }
            out.push('>');
            out.push_str(&serialize_children(node, Some(name.local.as_ref())));
            out.push_str("</");
            out.push_str(&tag);
            out.push('>');
        }
    }
}

fn serialize_attributes(node: &Handle, name: &QualName) -> String {
    let mut out = String::new();
    if let NodeData::Element { attrs, .. } = &node.data {
        for attr in attrs.borrow().iter() {
            out.push(' ');
            out.push_str(&qualified_name(&attr.name));
            out.push_str("=\"");
            out.push_str(&escape_attribute(&attr.value));
            out.push('"');
        }
    }
    let _ = name;
    out
}

fn qualified_name(name: &QualName) -> String {
    match name.prefix.as_ref() {
        Some(prefix) => format!("{}:{}", prefix, name.local),
        None => name.local.to_string(),
    }
}

pub fn opening_tag(node: &Handle, self_closing: bool) -> String {
    let Some(name) = element_name(node) else {
        return String::new();
    };
    let mut out = String::new();
    out.push('<');
    out.push_str(&name);
    if let NodeData::Element { attrs, .. } = &node.data {
        for attr in attrs.borrow().iter() {
            out.push(' ');
            out.push_str(&qualified_name(&attr.name));
            out.push_str("=\"");
            out.push_str(&escape_attribute(&attr.value));
            out.push('"');
        }
    }
    out.push_str(if self_closing { "/>" } else { ">" });
    out
}

fn escape_attribute(text: &str) -> String {
    escape_html(text).replace('"', "&quot;")
}

/// 合并连续空白为单个空格（保留首尾由调用方 trim）。
pub fn normalize_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const XHTML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="fr" xml:lang="fr">
<head><title>Chapitre</title><link rel="stylesheet" href="style/main.css"/></head>
<body><p>Il dit <i>bonjour</i> à M<sup>me</sup> Arnoux.<br/></p></body>
</html>"#;

    #[test]
    fn roundtrip_keeps_xhtml_structure() {
        let root = parse_document_html(XHTML.as_bytes());
        let out = serialize_document(&root);
        assert!(out.contains("xmlns=\"http://www.w3.org/1999/xhtml\""));
        assert!(out.contains("xmlns:epub=\"http://www.idpf.org/2007/ops\""));
        assert!(out.contains("<title>Chapitre</title>"));
        assert!(out.contains("<link rel=\"stylesheet\" href=\"style/main.css\"/>"));
        assert!(out.contains("<i>bonjour</i>"));
        assert!(out.contains("<br/>"));
        let options = roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let parsed =
            roxmltree::Document::parse_with_options(&out, options).expect("输出必须是合法 XML");
        assert_eq!(parsed.root_element().tag_name().name(), "html");
    }

    #[test]
    fn fragment_children_are_extractable() {
        let nodes = parse_fragment_html("<i>你好</i> 世界");
        assert_eq!(nodes.len(), 2);
        let text = serialize_nodes(&nodes);
        assert!(text.contains("<i>你好</i>"));
    }

    #[test]
    fn attribute_helpers_work() {
        let root = parse_document_html(XHTML.as_bytes());
        let p = find_first_element(&root, "p").expect("p");
        set_attribute(&p, "data-bc-seg", "s1");
        assert_eq!(get_attribute(&p, "data-bc-seg").as_deref(), Some("s1"));
        remove_attribute(&p, "data-bc-seg");
        assert!(get_attribute(&p, "data-bc-seg").is_none());
        let out = serialize_document(&root);
        let options = roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        roxmltree::Document::parse_with_options(&out, options).unwrap();
    }
}
