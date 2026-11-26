use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bookcraft::config::Settings;
use bookcraft::entity_matcher::MatchedEntities;
use bookcraft::llm::{
    BillingError, Completer, CompletionRequest, LlmResponse, LlmService, error_is_billing,
    is_billing_response,
};
use bookcraft::models::SegmentState;
use bookcraft::prompts::PromptLibrary;
use bookcraft::qa_gate::QAGate;
use bookcraft::state::{SegmentRecord, StateStore};

struct BillingCompleter;

#[async_trait]
impl Completer for BillingCompleter {
    async fn complete(&self, _request: &CompletionRequest) -> anyhow::Result<LlmResponse> {
        Err(BillingError("judge 额度耗尽".into()).into())
    }
}

fn settings(workdir: &std::path::Path) -> Arc<Settings> {
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/settings.yaml");
    let mut settings = Settings::load(Some(&config)).expect("config");
    settings.paths.workdir = workdir.to_string_lossy().to_string();
    settings.book.target_lang = "zh".into();
    settings.apply_mock();
    Arc::new(settings)
}

#[test]
fn detects_payment_required() {
    assert!(is_billing_response(402, ""));
}

#[test]
fn detects_quota_markers() {
    assert!(is_billing_response(
        429,
        r#"{"error":{"type":"insufficient_quota"}}"#
    ));
    assert!(is_billing_response(429, "账户余额不足，请充值"));
    assert!(is_billing_response(429, "额度已用尽"));
}

#[test]
fn plain_rate_limit_is_not_billing() {
    assert!(!is_billing_response(429, "rate limit exceeded"));
    assert!(!is_billing_response(529, "overloaded"));
}

#[tokio::test]
async fn billing_error_is_not_fail_open_in_judge() {
    let dir = tempfile::tempdir().expect("temp");
    let settings = settings(dir.path());
    let store = Arc::new(StateStore::open(dir.path().join("state.db")).expect("store"));
    let llm = Arc::new(
        LlmService::with_override(settings.clone(), store, "mock", Arc::new(BillingCompleter))
            .expect("llm"),
    );
    let prompts = Arc::new(PromptLibrary::new(&settings.prompts_dir).expect("prompts"));
    let gate = QAGate::new(settings, prompts).expect("gate");

    let segment = SegmentRecord {
        id: "ch_001_s0001".into(),
        chapter_id: "ch_001".into(),
        order_index: 1,
        source_text: "Il dit bonjour.".into(),
        source_html: String::new(),
        tag_map: Default::default(),
        source_words: 3,
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
    };
    let empty = MatchedEntities::default();
    let error = gate
        .evaluate(
            &llm,
            &segment,
            "他说你好。",
            &empty,
            &empty,
            &HashSet::new(),
            &[],
        )
        .await
        .expect_err("billing error must propagate");
    assert!(error_is_billing(&error));
}
