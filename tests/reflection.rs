use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use bookcraft::config::Settings;
use bookcraft::llm::{Completer, CompletionRequest, LlmResponse, LlmService};
use bookcraft::models::{SegmentState, TranslationBible};
use bookcraft::prompts::PromptLibrary;
use bookcraft::state::{ChapterRecord, SegmentRecord, StateStore};
use bookcraft::tags::{TagEntry, TagMap};
use bookcraft::translator::{TranslationEngine, TranslationOptions};

const SOURCE: &str = "Il dit [tag_1]bonjour[/tag_1] à Arnoux.";
const GOOD_1: &str = "他对阿尔努先生说[tag_1]你好[/tag_1]。";
const GOOD_2: &str = "他向阿尔努先生问[tag_1]好[/tag_1]。";
const BAD_TAG_ONLY: &str = "他对阿尔努先生问好。";

fn tag_map() -> TagMap {
    HashMap::from([(
        "tag_1".to_string(),
        TagEntry {
            open: "<i>".into(),
            close: "</i>".into(),
        },
    )])
}

struct ScriptedCompleter {
    pass2: Mutex<HashMap<String, Vec<String>>>,
    judge: Mutex<HashMap<String, Vec<Value>>>,
    counts: Mutex<HashMap<(String, String), usize>>,
    pass1_prompts: Mutex<Vec<String>>,
    pass2_prompts: Mutex<Vec<String>>,
}

impl ScriptedCompleter {
    fn new(pass2: Vec<(&str, Vec<&str>)>, judge: Vec<(&str, Vec<Value>)>) -> Self {
        Self {
            pass2: Mutex::new(
                pass2
                    .into_iter()
                    .map(|(id, values)| {
                        (
                            id.to_string(),
                            values.into_iter().map(str::to_string).collect(),
                        )
                    })
                    .collect(),
            ),
            judge: Mutex::new(
                judge
                    .into_iter()
                    .map(|(id, values)| (id.to_string(), values))
                    .collect(),
            ),
            counts: Mutex::new(HashMap::new()),
            pass1_prompts: Mutex::new(Vec::new()),
            pass2_prompts: Mutex::new(Vec::new()),
        }
    }

    fn next_index(&self, stage: &str, segment_id: &str) -> usize {
        let mut counts = self.counts.lock().unwrap();
        let entry = counts
            .entry((stage.to_string(), segment_id.to_string()))
            .or_insert(0);
        let index = *entry;
        *entry += 1;
        index
    }

    fn pass1_calls(&self) -> usize {
        self.pass1_prompts.lock().unwrap().len()
    }

    fn pass2_calls(&self) -> usize {
        self.pass2_prompts.lock().unwrap().len()
    }

    fn pass2_prompt(&self, index: usize) -> String {
        self.pass2_prompts.lock().unwrap()[index].clone()
    }
}

#[async_trait]
impl Completer for ScriptedCompleter {
    async fn complete(&self, request: &CompletionRequest) -> Result<LlmResponse> {
        let stage = request
            .meta
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or("");
        let segment_id = request
            .meta
            .get("segment_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let source = request
            .meta
            .get("source_text")
            .and_then(Value::as_str)
            .unwrap_or(SOURCE);
        let index = self.next_index(stage, segment_id);

        let value = match stage {
            "pass1" => {
                self.pass1_prompts
                    .lock()
                    .unwrap()
                    .push(request.user.clone());
                json!({
                    "syntax_and_nuance_notes": "代词 il 指 Frédéric",
                    "faithful_draft": source,
                })
            }
            "pass2" => {
                self.pass2_prompts
                    .lock()
                    .unwrap()
                    .push(request.user.clone());
                let outputs = self.pass2.lock().unwrap();
                let text = outputs
                    .get(segment_id)
                    .and_then(|values| values.get(index).cloned())
                    .unwrap_or_else(|| GOOD_1.to_string());
                json!({"revision_thought": "test", "literary_translation": text})
            }
            "judge" => {
                let verdicts = self.judge.lock().unwrap();
                verdicts
                    .get(segment_id)
                    .and_then(|values| values.get(index).cloned())
                    .unwrap_or_else(
                        || json!({"score": 5, "pass": true, "error_routing": "NONE", "issues": []}),
                    )
            }
            other => return Err(anyhow!("未知 stage: {other}")),
        };

        Ok(LlmResponse {
            text: value.to_string(),
            provider: "mock".into(),
            model: "scripted".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
        })
    }
}

struct ExplodingCompleter {
    calls: AtomicUsize,
}

#[async_trait]
impl Completer for ExplodingCompleter {
    async fn complete(&self, _request: &CompletionRequest) -> Result<LlmResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(anyhow!("不应重新处理 QA_PASSED 段落"))
    }
}

fn settings(workdir: &Path) -> Arc<Settings> {
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/settings.yaml");
    let mut settings = Settings::load(Some(&config)).expect("config");
    settings.paths.workdir = workdir.to_string_lossy().to_string();
    settings.qa.max_retry = 2;
    settings.book.target_lang = "zh".into();
    settings.apply_mock();
    Arc::new(settings)
}

fn make_store(path: &Path, count: usize) -> Arc<StateStore> {
    let store = StateStore::open(path.join("state.db")).expect("store");
    let chapter = ChapterRecord {
        id: "ch_001".into(),
        order_index: 1,
        doc_name: "chap_01.xhtml".into(),
        title: "Chapitre premier".into(),
        template_path: "/tmp/nonexistent.xhtml".into(),
        status: "PENDING".into(),
    };
    let segments: Vec<SegmentRecord> = (1..=count)
        .map(|index| SegmentRecord {
            id: format!("ch_001_s{index:04}"),
            chapter_id: "ch_001".into(),
            order_index: index as i64,
            source_text: SOURCE.into(),
            source_html: "Il dit <i>bonjour</i> à Arnoux.".into(),
            tag_map: tag_map(),
            source_words: 5,
            is_dialogue: false,
            state: SegmentState::Pending,
            pass1_notes: None,
            pass1_draft: None,
            final_text: None,
            best_text: None,
            qa_score: None,
            last_routing: None,
            last_error: None,
            retry_count: 0,
            updated_at: String::new(),
        })
        .collect();
    store
        .replace_content(&[chapter], &segments)
        .expect("replace content");
    Arc::new(store)
}

fn make_engine(
    settings: Arc<Settings>,
    store: Arc<StateStore>,
    completer: Arc<dyn Completer>,
) -> TranslationEngine {
    let prompts = Arc::new(PromptLibrary::new(&settings.prompts_dir).expect("prompts"));
    let llm = Arc::new(
        LlmService::with_override(settings.clone(), store.clone(), "mock", completer)
            .expect("llm service"),
    );
    TranslationEngine::new(settings, store, llm, prompts, TranslationBible::default())
        .expect("engine")
}

fn qa_report_count(dir: &Path) -> i64 {
    let conn = rusqlite::Connection::open(dir.join("state.db")).expect("db");
    conn.query_row("SELECT COUNT(*) FROM qa_reports", [], |row| row.get(0))
        .expect("count")
}

#[tokio::test]
async fn level1_failure_reruns_pass2_only() {
    let dir = tempfile::tempdir().expect("temp");
    let settings = settings(dir.path());
    let store = make_store(dir.path(), 1);
    let scripted = Arc::new(ScriptedCompleter::new(
        vec![("ch_001_s0001", vec![BAD_TAG_ONLY, GOOD_1])],
        Vec::new(),
    ));
    let engine = make_engine(settings, store.clone(), scripted.clone());
    engine
        .run(TranslationOptions::default())
        .await
        .expect("run");

    let segment = store.get_segment("ch_001_s0001").unwrap().unwrap();
    assert_eq!(segment.state, SegmentState::QaPassed);
    assert_eq!(segment.retry_count, 1);
    assert_eq!(scripted.pass1_calls(), 1);
    assert_eq!(scripted.pass2_calls(), 2);
    assert!(scripted.pass2_prompt(1).contains("TAG_ERROR"));
    assert!(scripted.pass2_prompt(1).contains("占位符"));
    assert_eq!(qa_report_count(dir.path()), 2);
}

#[tokio::test]
async fn judge_failure_can_rerun_pass2() {
    let dir = tempfile::tempdir().expect("temp");
    let settings = settings(dir.path());
    let store = make_store(dir.path(), 1);
    let judge = vec![
        json!({
            "score": 3,
            "pass": false,
            "error_routing": "RERUN_PASS2_ONLY",
            "issues": [{
                "type": "TRANSLATIONESE",
                "source_span": "Il dit",
                "translation_span": "他说",
                "fix_instruction": "拆掉欧化句式，改成更自然的短句"
            }]
        }),
        json!({"score": 5, "pass": true, "error_routing": "NONE", "issues": []}),
    ];
    let scripted = Arc::new(ScriptedCompleter::new(
        vec![("ch_001_s0001", vec![GOOD_1, GOOD_2])],
        vec![("ch_001_s0001", judge)],
    ));
    let engine = make_engine(settings, store.clone(), scripted.clone());
    engine
        .run(TranslationOptions::default())
        .await
        .expect("run");

    let segment = store.get_segment("ch_001_s0001").unwrap().unwrap();
    assert_eq!(segment.state, SegmentState::QaPassed);
    assert_eq!(scripted.pass1_calls(), 1);
    assert_eq!(scripted.pass2_calls(), 2);
    assert!(scripted.pass2_prompt(1).contains("拆掉欧化句式"));
}

#[tokio::test]
async fn judge_hard_error_reruns_from_pass1() {
    let dir = tempfile::tempdir().expect("temp");
    let settings = settings(dir.path());
    let store = make_store(dir.path(), 1);
    let judge = vec![
        json!({
            "score": 2,
            "pass": false,
            "error_routing": "RERUN_FROM_PASS1",
            "issues": [{
                "type": "MISTRANSLATION",
                "source_span": "bonjour",
                "translation_span": "再见",
                "fix_instruction": "bonjour 应译为“你好”"
            }]
        }),
        json!({"score": 5, "pass": true, "error_routing": "NONE", "issues": []}),
    ];
    let scripted = Arc::new(ScriptedCompleter::new(
        vec![("ch_001_s0001", vec![GOOD_1, GOOD_2])],
        vec![("ch_001_s0001", judge)],
    ));
    let engine = make_engine(settings, store.clone(), scripted.clone());
    engine
        .run(TranslationOptions::default())
        .await
        .expect("run");

    assert_eq!(scripted.pass1_calls(), 2);
    assert_eq!(scripted.pass2_calls(), 2);
    assert_eq!(
        store.get_segment("ch_001_s0001").unwrap().unwrap().state,
        SegmentState::QaPassed
    );
}

#[tokio::test]
async fn retry_exhaustion_marks_human_and_continues() {
    let dir = tempfile::tempdir().expect("temp");
    let settings = settings(dir.path());
    let store = make_store(dir.path(), 2);
    let scripted = Arc::new(ScriptedCompleter::new(
        vec![
            (
                "ch_001_s0001",
                vec![BAD_TAG_ONLY, BAD_TAG_ONLY, BAD_TAG_ONLY],
            ),
            ("ch_001_s0002", vec![GOOD_1]),
        ],
        Vec::new(),
    ));
    let engine = make_engine(settings, store.clone(), scripted);
    engine
        .run(TranslationOptions::default())
        .await
        .expect("run");

    let first = store.get_segment("ch_001_s0001").unwrap().unwrap();
    let second = store.get_segment("ch_001_s0002").unwrap().unwrap();
    assert_eq!(first.state, SegmentState::HumanReview);
    assert_eq!(first.retry_count, 3);
    assert!(first.last_error.unwrap().contains("占位符"));
    assert_eq!(second.state, SegmentState::QaPassed);
}

#[tokio::test]
async fn resume_skips_passed_segments() {
    let dir = tempfile::tempdir().expect("temp");
    let settings = settings(dir.path());
    let store = make_store(dir.path(), 1);
    let scripted = Arc::new(ScriptedCompleter::new(
        vec![("ch_001_s0001", vec![GOOD_1])],
        Vec::new(),
    ));
    let engine = make_engine(settings.clone(), store.clone(), scripted);
    engine
        .run(TranslationOptions::default())
        .await
        .expect("run");

    let exploding = Arc::new(ExplodingCompleter {
        calls: AtomicUsize::new(0),
    });
    let engine = make_engine(settings, store.clone(), exploding.clone());
    engine
        .run(TranslationOptions::default())
        .await
        .expect("second run");
    assert_eq!(exploding.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.get_segment("ch_001_s0001").unwrap().unwrap().state,
        SegmentState::QaPassed
    );
}
