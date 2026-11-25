use std::collections::HashMap;

use bookcraft::tags::{
    MarkerKind, TagEntry, TagMap, parse_markers, restore_tags, strip_tags, tag_counts,
    validate_tags,
};

fn tag_map() -> TagMap {
    HashMap::from([
        (
            "tag_1".to_string(),
            TagEntry {
                open: "<i>".into(),
                close: "</i>".into(),
            },
        ),
        (
            "tag_2".to_string(),
            TagEntry {
                open: "<a class=\"note\" href=\"#n1\">".into(),
                close: "</a>".into(),
            },
        ),
        (
            "tag_3".to_string(),
            TagEntry {
                open: "<br/>".into(),
                close: String::new(),
            },
        ),
    ])
}

#[test]
fn parse_marker_kinds() {
    let markers = parse_markers("a[tag_1]b[/tag_1]c[tag_3/]");
    let kinds: Vec<(MarkerKind, &str)> = markers
        .iter()
        .map(|marker| (marker.kind, marker.tag_id.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (MarkerKind::Open, "tag_1"),
            (MarkerKind::Close, "tag_1"),
            (MarkerKind::Void, "tag_3"),
        ]
    );
}

#[test]
fn tag_counts_work() {
    let counts = tag_counts("[tag_1]x[/tag_1][tag_3/]");
    assert_eq!(counts[&("tag_1".into(), MarkerKind::Open)], 1);
    assert_eq!(counts[&("tag_1".into(), MarkerKind::Close)], 1);
    assert_eq!(counts[&("tag_3".into(), MarkerKind::Void)], 1);
}

#[test]
fn validate_accepts_balanced_translation() {
    let source = "Il dit [tag_1]bonjour[/tag_1] à M[tag_2]me[/tag_2] Arnoux.";
    let translation = "他说[tag_1]你好[/tag_1]，[tag_2]夫[/tag_2]人。";
    assert!(validate_tags(source, translation, &tag_map()).is_empty());
}

#[test]
fn missing_tag_detected() {
    let errors = validate_tags("Il dit [tag_1]bonjour[/tag_1].", "他说你好。", &tag_map());
    assert!(errors.iter().any(|error| error.contains("数量不一致")));
}

#[test]
fn wrong_nesting_detected() {
    let errors = validate_tags("[tag_1]a[/tag_1]", "[tag_1]a", &tag_map());
    assert!(errors.iter().any(|error| error.contains("未闭合")));
}

#[test]
fn unknown_marker_detected() {
    let errors = validate_tags("A", "甲[tag_9]乙", &tag_map());
    assert!(errors.iter().any(|error| error.contains("未定义")));
}

#[test]
fn restore_wraps_and_escapes() {
    let restored = restore_tags(
        "A & B [tag_1]<x>[/tag_1] [tag_2]注[/tag_2][tag_3/]",
        &tag_map(),
    );
    assert_eq!(
        restored,
        "A &amp; B <i>&lt;x&gt;</i> <a class=\"note\" href=\"#n1\">注</a><br/>"
    );
}

#[test]
fn restore_unknown_marker_is_escaped() {
    assert_eq!(restore_tags("[tag_8]x", &tag_map()), "[tag_8]x");
}

#[test]
fn strip_markers() {
    assert_eq!(strip_tags("[tag_1]你[/tag_1]好[tag_2/]"), "你好");
}
