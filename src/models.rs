//! Pydantic 模型的 Rust 对应物：翻译圣经、阶段产物、QA 裁决与状态枚举。

use serde::{Deserialize, Serialize};

/// LLM 结构化输出共用的校验接口。
pub trait Validate {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SegmentState {
    Pending,
    Drafted,
    Polished,
    QaPassed,
    QaFailed,
    HumanReview,
}

impl SegmentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Drafted => "DRAFTED",
            Self::Polished => "POLISHED",
            Self::QaPassed => "QA_PASSED",
            Self::QaFailed => "QA_FAILED",
            Self::HumanReview => "HUMAN_REVIEW",
        }
    }
}

impl std::fmt::Display for SegmentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SegmentState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "PENDING" => Ok(Self::Pending),
            "DRAFTED" => Ok(Self::Drafted),
            "POLISHED" => Ok(Self::Polished),
            "QA_PASSED" => Ok(Self::QaPassed),
            "QA_FAILED" => Ok(Self::QaFailed),
            "HUMAN_REVIEW" => Ok(Self::HumanReview),
            other => Err(format!("未知段落状态: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorRouting {
    #[default]
    None,
    RerunPass2Only,
    RerunFromPass1,
}

impl ErrorRouting {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::RerunPass2Only => "RERUN_PASS2_ONLY",
            Self::RerunFromPass1 => "RERUN_FROM_PASS1",
        }
    }
}

impl std::str::FromStr for ErrorRouting {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "NONE" => Ok(Self::None),
            "RERUN_PASS2_ONLY" => Ok(Self::RerunPass2Only),
            "RERUN_FROM_PASS1" => Ok(Self::RerunFromPass1),
            other => Err(format!("未知错误路由: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueType {
    Omission,
    Mistranslation,
    Translationese,
    GlossaryDrift,
    VoiceMismatch,
    TagError,
    LengthAnomaly,
}

impl IssueType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Omission => "OMISSION",
            Self::Mistranslation => "MISTRANSLATION",
            Self::Translationese => "TRANSLATIONESE",
            Self::GlossaryDrift => "GLOSSARY_DRIFT",
            Self::VoiceMismatch => "VOICE_MISMATCH",
            Self::TagError => "TAG_ERROR",
            Self::LengthAnomaly => "LENGTH_ANOMALY",
        }
    }
}

// ---------------------------------------------------------------------------
// Translation Bible
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PronounRule {
    #[serde(default)]
    pub target_character: String,
    #[serde(default)]
    pub pronoun: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Character {
    pub canonical_id: String,
    pub source_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub target_name: String,
    #[serde(default)]
    pub short_target_map: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub forbidden_variants: Vec<String>,
    #[serde(default)]
    pub identity_and_arc: String,
    #[serde(default)]
    pub voice_profile: String,
    #[serde(default)]
    pub pronoun_matrix: Vec<PronounRule>,
}

impl Character {
    pub fn all_source_forms(&self) -> Vec<String> {
        let mut forms = vec![self.source_name.clone()];
        forms.extend(self.aliases.iter().cloned());
        forms.extend(self.short_target_map.keys().cloned());
        dedupe(forms)
    }

    pub fn all_allowed_targets(&self) -> Vec<String> {
        let mut forms = vec![self.target_name.clone()];
        forms.extend(self.short_target_map.values().cloned());
        dedupe(forms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlossaryEntry {
    #[serde(default)]
    pub id: String,
    pub source_term: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub target_term: String,
    #[serde(default)]
    pub forbidden_variants: Vec<String>,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub context_note: String,
}

impl GlossaryEntry {
    pub fn all_source_forms(&self) -> Vec<String> {
        let mut forms = vec![self.source_term.clone()];
        forms.extend(self.aliases.iter().cloned());
        dedupe(forms)
    }

    pub fn all_allowed_targets(&self) -> Vec<String> {
        if self.target_term.is_empty() {
            Vec::new()
        } else {
            vec![self.target_term.clone()]
        }
    }
}

fn default_category() -> String {
    "OTHER".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalStyleGuide {
    #[serde(default)]
    pub narrative_tone: String,
    #[serde(default)]
    pub free_indirect_discourse_rule: String,
    #[serde(default)]
    pub syntax_strategy: String,
    #[serde(default)]
    pub honorifics_rules: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TranslationBible {
    #[serde(default)]
    pub global_style_guide: GlobalStyleGuide,
    #[serde(default)]
    pub core_characters: Vec<Character>,
    #[serde(default)]
    pub glossary: Vec<GlossaryEntry>,
}

impl Validate for TranslationBible {}

// ---------------------------------------------------------------------------
// Pass 1 / Pass 2
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pass1Result {
    #[serde(default)]
    pub syntax_and_nuance_notes: String,
    pub faithful_draft: String,
}

impl Validate for Pass1Result {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pass2Result {
    #[serde(default)]
    pub revision_thought: String,
    pub literary_translation: String,
}

impl Validate for Pass2Result {}

// ---------------------------------------------------------------------------
// Judge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaIssue {
    #[serde(rename = "type")]
    pub issue_type: IssueType,
    #[serde(default)]
    pub source_span: String,
    #[serde(default)]
    pub translation_span: String,
    #[serde(default)]
    pub fix_instruction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeVerdict {
    pub score: i32,
    pub pass: bool,
    #[serde(default)]
    pub error_routing: ErrorRouting,
    #[serde(default)]
    pub issues: Vec<QaIssue>,
}

impl Validate for JudgeVerdict {
    fn validate(&self) -> Result<(), String> {
        if !(1..=5).contains(&self.score) {
            return Err(format!("score 必须在 1~5 之间，实际为 {}", self.score));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Book metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BookMeta {
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default = "default_source_lang")]
    pub source_lang: String,
    #[serde(default = "default_target_lang")]
    pub target_lang: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub source_sha256: String,
    #[serde(default)]
    pub chapter_count: i64,
    #[serde(default)]
    pub segment_count: i64,
}

fn default_source_lang() -> String {
    "fr".to_string()
}

fn default_target_lang() -> String {
    "zh".to_string()
}

pub fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for value in values {
        if !value.is_empty() && seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}
