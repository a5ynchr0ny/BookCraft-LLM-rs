use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use bookcraft::export_validate::{validate_epub, validate_segments};
use bookcraft::models::SegmentState;
use bookcraft::state::SegmentRecord;
use bookcraft::tags::{TagEntry, TagMap};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const CONTAINER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

const OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">test</dc:identifier>
    <dc:title>T</dc:title>
    <dc:language>zh</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="ch1"/></spine>
</package>"#;

const NCX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1">
      <navLabel><text>Ch</text></navLabel>
      <content src="ch1.xhtml"/>
    </navPoint>
  </navMap>
</ncx>"#;

fn make_epub(path: &Path, body: &str) {
    let file = File::create(path).expect("create");
    let mut zip = ZipWriter::new(file);
    zip.start_file(
        "mimetype",
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )
    .expect("mimetype");
    zip.write_all(b"application/epub+zip")
        .expect("mimetype body");

    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, content) in [
        ("META-INF/container.xml", CONTAINER.to_string()),
        ("OEBPS/content.opf", OPF.to_string()),
        ("OEBPS/toc.ncx", NCX.to_string()),
        (
            "OEBPS/ch1.xhtml",
            format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#),
        ),
    ] {
        zip.start_file(name, deflated).expect("entry");
        zip.write_all(content.as_bytes()).expect("entry body");
    }
    zip.finish().expect("finish");
}

fn tag_map() -> TagMap {
    HashMap::from([(
        "tag_1".to_string(),
        TagEntry {
            open: "<i>".into(),
            close: "</i>".into(),
        },
    )])
}

fn make_segment(text: Option<&str>, tag_map: TagMap) -> SegmentRecord {
    SegmentRecord {
        id: "s1".into(),
        chapter_id: "c1".into(),
        order_index: 1,
        source_text: "[tag_1]x[/tag_1]".into(),
        source_html: String::new(),
        tag_map,
        source_words: 1,
        is_dialogue: false,
        state: SegmentState::Pending,
        pass1_notes: None,
        pass1_draft: None,
        final_text: text.map(str::to_string),
        best_text: None,
        qa_score: None,
        last_routing: None,
        last_error: None,
        retry_count: 0,
        updated_at: String::new(),
    }
}

#[test]
fn valid_segments_pass() {
    assert!(validate_segments(&[make_segment(Some("[tag_1]译文[/tag_1]"), tag_map())]).is_empty());
}

#[test]
fn missing_translation_is_rejected() {
    assert!(!validate_segments(&[make_segment(None, tag_map())]).is_empty());
    assert!(!validate_segments(&[make_segment(Some("   "), tag_map())]).is_empty());
}

#[test]
fn unknown_placeholder_is_rejected() {
    let problems = validate_segments(&[make_segment(Some("[tag_1]译文[/tag_1]"), HashMap::new())]);
    assert!(!problems.is_empty());
}

#[test]
fn valid_epub_passes() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("ok.epub");
    make_epub(&path, "<p>正文</p>");
    assert_eq!(validate_epub(&path), Vec::<String>::new());
}

#[test]
fn placeholder_and_broken_link_are_rejected() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("bad.epub");
    make_epub(
        &path,
        r#"<p>[tag_1]正文[/tag_1]</p><img src="missing.jpg"/>"#,
    );
    let problems = validate_epub(&path);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("残留占位符")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("相对链接失效")),
        "{problems:?}"
    );
}
