mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::build_sample_epub;

fn run_cli(workdir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bookcraft"))
        .arg("--workdir")
        .arg(workdir)
        .args(args)
        .output()
        .expect("run bookcraft")
}

fn count(workdir: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(workdir.join("state.db")).expect("db");
    conn.query_row(sql, [], |row| row.get(0)).expect("query")
}

fn passed_count(workdir: &Path) -> i64 {
    count(
        workdir,
        "SELECT COUNT(*) FROM segments WHERE state='QA_PASSED'",
    )
}

fn find_output(workdir: &Path, suffix: &str) -> std::path::PathBuf {
    std::fs::read_dir(workdir.join("output"))
        .expect("output dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.to_string_lossy().ends_with(suffix))
        .unwrap_or_else(|| panic!("missing output {suffix}"))
}

fn assert_epub(path: &Path, bilingual: bool) {
    let file = std::fs::File::open(path).expect("open epub");
    let mut archive = zip::ZipArchive::new(file).expect("zip");
    assert!(archive.index_for_name("mimetype").is_some());
    {
        let mimetype = archive.by_name("mimetype").expect("mimetype");
        assert_eq!(mimetype.compression(), zip::CompressionMethod::Stored);
    }
    let mut xhtml = String::new();
    {
        use std::io::Read;
        let mut entry = archive.by_name("OEBPS/chap_01.xhtml").expect("chap_01");
        entry.read_to_string(&mut xhtml).expect("read xhtml");
    }
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    roxmltree::Document::parse_with_options(&xhtml, options).expect("valid XML");
    assert!(!xhtml.contains("[tag_"), "leftover placeholders");
    assert!(xhtml.contains("<i>"));
    assert!(xhtml.contains("<sup>"));
    assert!(xhtml.contains("class=\"note\""));
    assert!(xhtml.contains("<br/>"));
    assert!(xhtml.contains("文字"));
    if bilingual {
        assert!(xhtml.contains("bc-zh"));
        assert!(xhtml.contains("bc-src"));
    }

    let mut opf = String::new();
    {
        use std::io::Read;
        let mut entry = archive.by_name("OEBPS/content.opf").expect("opf");
        entry.read_to_string(&mut opf).expect("read opf");
    }
    assert!(opf.contains(if bilingual {
        "双语对照版"
    } else {
        "中文精译版"
    }));
    assert!(opf.contains("<dc:language>zh</dc:language>"));
}

#[test]
fn one_shot_mock_pipeline_exports_valid_epubs() {
    let temp = tempfile::tempdir().expect("temp");
    let epub = build_sample_epub(&temp.path().join("sample.epub")).expect("sample epub");
    let workdir = temp.path().join("work");

    let output = run_cli(&workdir, &["--mock", "run", epub.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(passed_count(&workdir), 9);

    let zh = find_output(&workdir, ".zh.epub");
    let bilingual = find_output(&workdir, ".bilingual.epub");
    assert_epub(&zh, false);
    assert_epub(&bilingual, true);
    assert!(workdir.join("output/qa_report.md").exists());
}

#[test]
fn resume_limit_and_idempotency() {
    let temp = tempfile::tempdir().expect("temp");
    let epub = build_sample_epub(&temp.path().join("sample.epub")).expect("sample epub");
    let workdir = temp.path().join("work");
    let epub_arg = epub.to_str().unwrap();

    let prepare = run_cli(&workdir, &["prepare", epub_arg]);
    assert!(prepare.status.success(), "{prepare:?}");
    let guard = run_cli(&workdir, &["prepare", epub_arg]);
    assert!(!guard.status.success(), "prepare without --force must fail");

    let bible = run_cli(&workdir, &["--mock", "bible"]);
    assert!(bible.status.success(), "{bible:?}");

    let first = run_cli(&workdir, &["--mock", "translate", "--limit", "3"]);
    assert!(first.status.success(), "{first:?}");
    assert_eq!(passed_count(&workdir), 3);

    let rest = run_cli(&workdir, &["--mock", "translate"]);
    assert!(rest.status.success(), "{rest:?}");
    assert_eq!(passed_count(&workdir), 9);

    let reports_before = count(&workdir, "SELECT COUNT(*) FROM qa_reports");
    let again = run_cli(&workdir, &["--mock", "translate"]);
    assert!(again.status.success(), "{again:?}");
    assert_eq!(
        count(&workdir, "SELECT COUNT(*) FROM qa_reports"),
        reports_before
    );

    let export = run_cli(&workdir, &["export"]);
    assert!(export.status.success(), "{export:?}");
    assert!(workdir.join("output/qa_report.md").exists());

    let status = run_cli(&workdir, &["status"]);
    assert!(status.status.success());
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("QA_PASSED=9"), "stdout: {stdout}");
}
