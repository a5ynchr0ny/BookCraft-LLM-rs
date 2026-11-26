//! Step 3：自动化质检门禁（QA Gate）。

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use regex::Regex;
use serde_json::json;
use tracing::warn;

use crate::config::Settings;
use crate::entity_matcher::MatchedEntities;
use crate::llm::LlmService;
use crate::models::{ErrorRouting, IssueType, JudgeVerdict, QaIssue};
use crate::prompts::{PromptLibrary, entities_json, join_paragraphs};
use crate::state::SegmentRecord;
use crate::tags::{strip_tags, validate_tags};

static HAN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\u{3400}-\u{9fff}]").unwrap());
static CJK_SOURCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\u{3040}-\u{30ff}\u{3400}-\u{9fff}\u{ac00}-\u{d7af}]").unwrap());

#[derive(Debug, Clone)]
pub struct QaResult {
    pub passed: bool,
    pub level1_passed: bool,
    pub score: Option<f64>,
    pub routing: ErrorRouting,
    pub issues: Vec<QaIssue>,
    pub judge_skipped_reason: Option<String>,
}

impl QaResult {
    pub fn issues_json(&self) -> String {
        serde_json::to_string(&self.issues).unwrap_or_else(|_| "[]".into())
    }

    pub fn feedback_text(&self) -> String {
        if self.issues.is_empty() {
            return self
                .judge_skipped_reason
                .as_ref()
                .map(|reason| format!("（Judge 跳过：{reason}）"))
                .unwrap_or_default();
        }
        self.issues
            .iter()
            .take(8)
            .map(|issue| {
                let mut parts = vec![format!("[{}]", issue.issue_type.as_str())];
                if !issue.source_span.is_empty() {
                    parts.push(format!("原文：{}", issue.source_span));
                }
                if !issue.translation_span.is_empty() {
                    parts.push(format!("译文：{}", issue.translation_span));
                }
                if !issue.fix_instruction.is_empty() {
                    parts.push(format!("修正：{}", issue.fix_instruction));
                }
                parts.join("；")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub struct QAGate {
    settings: Arc<Settings>,
    prompts: Arc<PromptLibrary>,
    judge_system: String,
}

impl QAGate {
    pub fn new(settings: Arc<Settings>, prompts: Arc<PromptLibrary>) -> Result<Self> {
        let judge_system = prompts.render("judge_system.j2", json!({}))?;
        Ok(Self {
            settings,
            prompts,
            judge_system,
        })
    }

    // ------------------------------------------------------------------
    // Level 1
    // ------------------------------------------------------------------
    pub fn check_deterministic(
        &self,
        segment: &SegmentRecord,
        translation: &str,
        mentioned: &MatchedEntities,
        first_keys: &HashSet<String>,
    ) -> (bool, Vec<QaIssue>, ErrorRouting) {
        let mut issues = Vec::new();

        for error in validate_tags(&segment.source_text, translation, &segment.tag_map) {
            issues.push(QaIssue {
                issue_type: IssueType::TagError,
                source_span: String::new(),
                translation_span: String::new(),
                fix_instruction: format!("{error}。请逐个保留原文中的 [tag_x] 占位符。"),
            });
        }

        let translated_text = strip_tags(translation).trim().to_string();
        if translated_text.is_empty() {
            issues.push(QaIssue {
                issue_type: IssueType::Omission,
                source_span: truncate(&segment.source_text, 120),
                translation_span: String::new(),
                fix_instruction: "本段译文为空，请完整翻译整段。".into(),
            });
            return (false, issues, ErrorRouting::RerunFromPass1);
        }

        issues.extend(self.check_glossary(translation, mentioned, first_keys));
        issues.extend(self.check_length_ratio(segment, translation));

        let routing = derive_routing(&issues);
        let passed = issues.is_empty();
        (passed, issues, routing)
    }

    fn check_glossary(
        &self,
        translation: &str,
        mentioned: &MatchedEntities,
        first_keys: &HashSet<String>,
    ) -> Vec<QaIssue> {
        let mut issues = Vec::new();

        for character in &mentioned.characters {
            let key = format!("char:{}", character.canonical_id);
            let masked = mask_allowed_targets(translation, &character.all_allowed_targets());
            for variant in &character.forbidden_variants {
                if contains_text(&masked, variant) {
                    issues.push(QaIssue {
                        issue_type: IssueType::GlossaryDrift,
                        source_span: character.source_name.clone(),
                        translation_span: variant.clone(),
                        fix_instruction: format!(
                            "将错误音译「{variant}」统一改为标准译名「{}」。",
                            character.target_name
                        ),
                    });
                }
            }
            if first_keys.contains(&key) {
                let allowed = character.all_allowed_targets();
                if !allowed.is_empty()
                    && !allowed
                        .iter()
                        .any(|target| contains_text(translation, target))
                {
                    issues.push(QaIssue {
                        issue_type: IssueType::GlossaryDrift,
                        source_span: character.source_name.clone(),
                        translation_span: String::new(),
                        fix_instruction: format!(
                            "「{}」在本章首次出现，必须使用标准译名「{}」，不得直接省译为代词。",
                            character.source_name, character.target_name
                        ),
                    });
                }
            }
        }

        for entry in &mentioned.glossary {
            let key = format!("gloss:{}", entry.source_term);
            let masked = mask_allowed_targets(translation, &entry.all_allowed_targets());
            for variant in &entry.forbidden_variants {
                if contains_text(&masked, variant) {
                    issues.push(QaIssue {
                        issue_type: IssueType::GlossaryDrift,
                        source_span: entry.source_term.clone(),
                        translation_span: variant.clone(),
                        fix_instruction: format!(
                            "将错误译名「{variant}」统一改为标准译名「{}」。",
                            entry.target_term
                        ),
                    });
                }
            }
            if first_keys.contains(&key)
                && !entry.target_term.is_empty()
                && !contains_text(translation, &entry.target_term)
            {
                issues.push(QaIssue {
                    issue_type: IssueType::GlossaryDrift,
                    source_span: entry.source_term.clone(),
                    translation_span: String::new(),
                    fix_instruction: format!(
                        "「{}」首次出现时必须使用标准译名「{}」。",
                        entry.source_term, entry.target_term
                    ),
                });
            }
        }
        issues
    }

    fn check_length_ratio(&self, segment: &SegmentRecord, translation: &str) -> Vec<QaIssue> {
        let target = self.settings.book.target_lang.to_lowercase();
        if !target.starts_with("zh") {
            return Vec::new();
        }
        let source = strip_tags(&segment.source_text);
        if CJK_SOURCE_RE.is_match(&source) || segment.source_words < 3 {
            return Vec::new();
        }

        let han_chars = HAN_RE.find_iter(&strip_tags(translation)).count() as f64;
        if han_chars == 0.0 {
            return vec![QaIssue {
                issue_type: IssueType::Omission,
                source_span: truncate(&source, 120),
                translation_span: String::new(),
                fix_instruction: "译文不含任何汉字，疑似模型未翻译或输出异常，请完整重译。".into(),
            }];
        }

        let ratio = han_chars / segment.source_words as f64;
        let config = &self.settings.qa.length_ratio;
        let (low, high) = if segment.is_dialogue {
            (config.dialogue_min, config.dialogue_max)
        } else {
            (config.min, config.max)
        };

        if ratio < low {
            return vec![QaIssue {
                issue_type: IssueType::Omission,
                source_span: truncate(&source, 120),
                translation_span: String::new(),
                fix_instruction: format!(
                    "译文 {han_chars} 汉字 / 原文 {} 词 = {ratio:.2}，低于正常区间下限 {low}，\
                     大概率漏译，请完整翻译每个从句与修饰语。",
                    segment.source_words
                ),
            }];
        }
        if ratio > high {
            return vec![QaIssue {
                issue_type: IssueType::LengthAnomaly,
                source_span: truncate(&source, 120),
                translation_span: String::new(),
                fix_instruction: format!(
                    "译文 {han_chars} 汉字 / 原文 {} 词 = {ratio:.2}，高于正常区间上限 {high}，\
                     疑似复读或过度扩写，请对照原文精简。",
                    segment.source_words
                ),
            }];
        }
        Vec::new()
    }

    // ------------------------------------------------------------------
    // Level 2 + 3
    // ------------------------------------------------------------------
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate(
        &self,
        llm: &LlmService,
        segment: &SegmentRecord,
        translation: &str,
        mentioned: &MatchedEntities,
        prompt_entities: &MatchedEntities,
        first_keys: &HashSet<String>,
        prev_translations: &[String],
    ) -> Result<QaResult> {
        let (level1_passed, issues, routing) =
            self.check_deterministic(segment, translation, mentioned, first_keys);
        if !level1_passed {
            return Ok(QaResult {
                passed: false,
                level1_passed: false,
                score: None,
                routing,
                issues,
                judge_skipped_reason: None,
            });
        }

        let user = match self.prompts.render(
            "judge_user.j2",
            json!({
                "active_entities_json": entities_json(prompt_entities),
                "prev_translations": join_paragraphs(prev_translations),
                "current_source": segment.source_text,
                "candidate_translation": translation,
            }),
        ) {
            Ok(user) => user,
            Err(error) => {
                warn!("[{}] 渲染 Judge Prompt 失败: {error:#}", segment.id);
                return Ok(QaResult {
                    passed: true,
                    level1_passed: true,
                    score: None,
                    routing: ErrorRouting::None,
                    issues: Vec::new(),
                    judge_skipped_reason: Some(format!("prompt render: {error:#}")),
                });
            }
        };

        let verdict = match llm
            .structured::<JudgeVerdict>(
                crate::config::RouteName::Judge,
                &self.judge_system,
                &user,
                "judge",
                json!({"source_text": segment.source_text, "candidate": translation}),
                Some(&segment.id),
            )
            .await
        {
            Ok(verdict) => verdict,
            Err(error) => {
                // 欠费/额度耗尽必须向上传播，绝不能 fail-open 成 QA_PASSED
                if crate::llm::error_is_billing(&error) {
                    return Err(error);
                }
                warn!("[{}] Judge 不可用，fail-open 放行: {error:#}", segment.id);
                return Ok(QaResult {
                    passed: true,
                    level1_passed: true,
                    score: None,
                    routing: ErrorRouting::None,
                    issues: Vec::new(),
                    judge_skipped_reason: Some(format!("{error:#}")),
                });
            }
        };

        let passed = verdict.pass && verdict.score >= self.settings.qa.judge_pass_score;
        let mut final_routing = ErrorRouting::None;
        if !passed {
            final_routing = if verdict.error_routing == ErrorRouting::None {
                let derived = derive_routing(&verdict.issues);
                if derived == ErrorRouting::None {
                    ErrorRouting::RerunPass2Only
                } else {
                    derived
                }
            } else {
                verdict.error_routing
            };
        }

        Ok(QaResult {
            passed,
            level1_passed: true,
            score: Some(verdict.score as f64),
            routing: final_routing,
            issues: verdict.issues,
            judge_skipped_reason: None,
        })
    }
}

pub fn derive_routing(issues: &[QaIssue]) -> ErrorRouting {
    let has_hard = issues.iter().any(|issue| {
        matches!(
            issue.issue_type,
            IssueType::Omission | IssueType::Mistranslation | IssueType::LengthAnomaly
        )
    });
    if has_hard {
        return ErrorRouting::RerunFromPass1;
    }
    let has_soft = issues.iter().any(|issue| {
        matches!(
            issue.issue_type,
            IssueType::TagError
                | IssueType::Translationese
                | IssueType::GlossaryDrift
                | IssueType::VoiceMismatch
        )
    });
    if has_soft {
        return ErrorRouting::RerunPass2Only;
    }
    ErrorRouting::None
}

fn mask_allowed_targets(text: &str, allowed_targets: &[String]) -> String {
    // 标准译名可能包含禁用变体作为子串（如「阿弗尔」之于「勒阿弗尔」），
    // 先遮掉允许译名，再检查剩余文本里是否真有禁用变体。
    let mut seen = HashSet::new();
    let mut targets: Vec<&String> = allowed_targets
        .iter()
        .filter(|target| !target.is_empty() && seen.insert(target.as_str()))
        .collect();
    targets.sort_by_key(|target| std::cmp::Reverse(target.chars().count()));

    let mut masked = text.to_string();
    for target in targets {
        if target.is_ascii() {
            masked = replace_ascii_case_insensitive(&masked, target, "\u{3000}");
        } else {
            masked = masked.replace(target.as_str(), "\u{3000}");
        }
    }
    masked
}

fn replace_ascii_case_insensitive(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let needle_bytes = needle.as_bytes();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < text.len() {
        let end = index + needle_bytes.len();
        if end <= text.len()
            && bytes[index..end].eq_ignore_ascii_case(needle_bytes)
            && text.is_char_boundary(end)
        {
            out.push_str(replacement);
            index = end;
            continue;
        }
        let ch = text[index..].chars().next().expect("char boundary");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn contains_text(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if needle.is_ascii() {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    } else {
        haystack.contains(needle)
    }
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
