use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use bookcraft::config::Settings;
use bookcraft::entity_matcher::MatchedEntities;
use bookcraft::models::{Character, ErrorRouting};
use bookcraft::prompts::PromptLibrary;
use bookcraft::qa_gate::QAGate;
use bookcraft::state::SegmentRecord;
use bookcraft::tags::{TagEntry, TagMap};

fn settings() -> Arc<Settings> {
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/settings.yaml");
    let mut settings = Settings::load(Some(&config)).expect("config");
    settings.book.target_lang = "zh".into();
    Arc::new(settings)
}

fn gate() -> QAGate {
    let settings = settings();
    let prompts = Arc::new(PromptLibrary::new(&settings.prompts_dir).expect("prompts"));
    QAGate::new(settings, prompts).expect("qa gate")
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

fn segment() -> SegmentRecord {
    SegmentRecord {
        id: "ch_001_s0001".into(),
        chapter_id: "ch_001".into(),
        order_index: 1,
        source_text: "Il dit [tag_1]bonjour[/tag_1] à Madame Arnoux.".into(),
        source_html: String::new(),
        tag_map: tag_map(),
        source_words: 6,
        is_dialogue: false,
        state: bookcraft::models::SegmentState::Pending,
        pass1_notes: None,
        pass1_draft: None,
        final_text: None,
        best_text: None,
        qa_score: None,
        last_routing: None,
        last_error: None,
        retry_count: 0,
        updated_at: String::new(),
    }
}

fn character() -> Character {
    Character {
        canonical_id: "char_001".into(),
        source_name: "Emile".into(),
        aliases: Vec::new(),
        target_name: "埃米尔".into(),
        short_target_map: BTreeMap::new(),
        forbidden_variants: vec!["艾米尔".into()],
        identity_and_arc: String::new(),
        voice_profile: String::new(),
        pronoun_matrix: Vec::new(),
    }
}

fn check(
    segment: &SegmentRecord,
    translation: &str,
    mentioned: &MatchedEntities,
    first: &HashSet<String>,
) -> (bool, Vec<bookcraft::models::QaIssue>, ErrorRouting) {
    gate().check_deterministic(segment, translation, mentioned, first)
}

#[test]
fn valid_translation_passes() {
    let (passed, issues, routing) = check(
        &segment(),
        "他对阿尔努夫人说[tag_1]你好[/tag_1]。",
        &MatchedEntities::default(),
        &HashSet::new(),
    );
    assert!(passed, "issues: {issues:?}");
    assert!(issues.is_empty());
    assert_eq!(routing, ErrorRouting::None);
}

#[test]
fn tag_error_routes_to_pass2() {
    let (passed, issues, routing) = check(
        &segment(),
        "他对阿尔努夫人说你好。",
        &MatchedEntities::default(),
        &HashSet::new(),
    );
    assert!(!passed);
    assert!(
        issues
            .iter()
            .any(|issue| issue.issue_type.as_str() == "TAG_ERROR")
    );
    assert_eq!(routing, ErrorRouting::RerunPass2Only);
}

#[test]
fn length_shortage_is_omission() {
    let mut segment = segment();
    segment.source_text = "Un deux trois quatre cinq six sept huit neuf dix.".into();
    segment.source_words = 10;
    let (passed, issues, routing) = check(
        &segment,
        "太短。",
        &MatchedEntities::default(),
        &HashSet::new(),
    );
    assert!(!passed);
    assert!(
        issues
            .iter()
            .any(|issue| issue.issue_type.as_str() == "OMISSION")
    );
    assert_eq!(routing, ErrorRouting::RerunFromPass1);
}

#[test]
fn length_overflow_is_anomaly() {
    let mut segment = segment();
    segment.source_text = "Un deux trois quatre.".into();
    segment.source_words = 4;
    let (passed, issues, routing) = check(
        &segment,
        &"字".repeat(20),
        &MatchedEntities::default(),
        &HashSet::new(),
    );
    assert!(!passed);
    assert!(
        issues
            .iter()
            .any(|issue| issue.issue_type.as_str() == "LENGTH_ANOMALY")
    );
    assert_eq!(routing, ErrorRouting::RerunFromPass1);
}

#[test]
fn forbidden_variant_detected() {
    let mentioned = MatchedEntities {
        characters: vec![character()],
        glossary: Vec::new(),
    };
    let (passed, issues, routing) = check(
        &segment(),
        "艾米尔说[tag_1]你好[/tag_1]，他很有礼貌。",
        &mentioned,
        &HashSet::new(),
    );
    assert!(!passed);
    assert!(
        issues
            .iter()
            .any(|issue| issue.issue_type.as_str() == "GLOSSARY_DRIFT")
    );
    assert_eq!(routing, ErrorRouting::RerunPass2Only);
}

#[test]
fn first_occurrence_requires_standard_name() {
    let mut character = character();
    character.forbidden_variants.clear();
    let mentioned = MatchedEntities {
        characters: vec![character],
        glossary: Vec::new(),
    };
    let mut first = HashSet::new();
    first.insert("char:char_001".to_string());
    let (passed, issues, _) = check(
        &segment(),
        "他走过来向[tag_1]夫人问好[/tag_1]。",
        &mentioned,
        &first,
    );
    assert!(!passed);
    assert!(
        issues
            .iter()
            .any(|issue| issue.fix_instruction.contains("首次出现"))
    );
}

#[test]
fn short_segment_skips_length_ratio() {
    let mut segment = segment();
    segment.source_text = "Oui.".into();
    segment.source_words = 1;
    let (passed, _, _) = check(
        &segment,
        "是。",
        &MatchedEntities::default(),
        &HashSet::new(),
    );
    assert!(passed);
}

#[test]
fn empty_translation_is_omission() {
    let (passed, _, routing) = check(
        &segment(),
        "   ",
        &MatchedEntities::default(),
        &HashSet::new(),
    );
    assert!(!passed);
    assert_eq!(routing, ErrorRouting::RerunFromPass1);
}
