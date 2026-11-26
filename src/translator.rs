//! Step 2：双步翻译调度器（章间并发 / 章内滑动窗口 / 反思重试）。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::json;
use tracing::{error, info, warn};

use crate::concurrency::AdaptiveSemaphore;
use crate::config::{RouteName, Settings};
use crate::entity_matcher::{EntityMatcher, MatchedEntities};
use crate::llm::{LlmService, error_is_billing};
use crate::models::{
    Character, ErrorRouting, Pass1Result, Pass2Result, SegmentState, TranslationBible,
};
use crate::prompts::{
    PromptLibrary, compact_entities_json, entities_json, join_paragraphs, style_guide_text,
};
use crate::qa_gate::QAGate;
use crate::state::{ChapterRecord, SegmentRecord, StateStore};

#[derive(Debug, Clone, Default)]
pub struct TranslationOptions {
    pub chapter_ids: Option<Vec<String>>,
    pub limit: Option<usize>,
    pub retry_human: bool,
}

struct RunState {
    slots: AtomicUsize,
    limit: Option<usize>,
    halt: AtomicBool,
}

impl RunState {
    fn take_slot(&self) -> bool {
        let previous = self.slots.fetch_add(1, Ordering::SeqCst);
        match self.limit {
            Some(limit) => previous < limit,
            None => true,
        }
    }
}

pub struct TranslationEngine {
    settings: Arc<Settings>,
    store: Arc<StateStore>,
    llm: Arc<LlmService>,
    prompts: Arc<PromptLibrary>,
    matcher: EntityMatcher,
    qa: QAGate,
    governor: Arc<AdaptiveSemaphore>,
    pass1_system: String,
    pass2_system: String,
}

impl TranslationEngine {
    pub fn new(
        settings: Arc<Settings>,
        store: Arc<StateStore>,
        llm: Arc<LlmService>,
        prompts: Arc<PromptLibrary>,
        bible: TranslationBible,
    ) -> Result<Self> {
        let matcher = EntityMatcher::new(&bible);
        let qa = QAGate::new(settings.clone(), prompts.clone())?;
        let governor = llm.governor();
        let pass1_system = prompts.render("pass1_system.j2", json!({}))?;
        let pass2_system = prompts.render(
            "pass2_system.j2",
            json!({"global_style_guide_text": style_guide_text(&bible)}),
        )?;
        Ok(Self {
            settings,
            store,
            llm,
            prompts,
            matcher,
            qa,
            governor,
            pass1_system,
            pass2_system,
        })
    }

    pub async fn run(&self, options: TranslationOptions) -> Result<crate::state::Stats> {
        self.llm.validate_routes()?;

        let mut chapters = self.store.list_chapters()?;
        if let Some(wanted) = &options.chapter_ids {
            let wanted_set: HashSet<&str> = wanted.iter().map(String::as_str).collect();
            chapters.retain(|chapter| {
                wanted_set.contains(chapter.id.as_str())
                    || wanted_set.contains(chapter.order_index.to_string().as_str())
            });
            if chapters.is_empty() {
                anyhow::bail!("没有匹配的章节: {:?}", wanted);
            }
        }

        let all_segments = self.store.list_segments(None)?;
        let texts: Vec<String> = all_segments
            .iter()
            .map(|segment| segment.source_text.clone())
            .collect();
        let core_characters = Arc::new(
            self.matcher
                .core_characters(&texts, self.settings.concurrency.core_characters),
        );
        if !core_characters.is_empty() {
            info!(
                "常驻核心角色：{}",
                core_characters
                    .iter()
                    .map(|character| character.target_name.clone())
                    .collect::<Vec<_>>()
                    .join("、")
            );
        }

        let stats_before = self.store.stats()?;
        info!(
            "开始翻译：{} 章 / {} 段（当前已通过 {} 段，章节并发 {}，{}）",
            chapters.len(),
            all_segments.len(),
            stats_before.states.get("QA_PASSED").copied().unwrap_or(0),
            self.governor.limit(),
            if self.governor.is_adaptive() {
                "自适应"
            } else {
                "固定"
            }
        );

        let state = Arc::new(RunState {
            slots: AtomicUsize::new(0),
            limit: options.limit,
            halt: AtomicBool::new(false),
        });
        let retry_human = options.retry_human;

        let mut tasks = FuturesUnordered::new();
        for chapter in chapters {
            let state = state.clone();
            let core = core_characters.clone();
            let governor = self.governor.clone();
            tasks.push(async move {
                let _permit = governor.acquire().await;
                let chapter_id = chapter.id.clone();
                let result = self.run_chapter(&chapter, &core, retry_human, &state).await;
                (chapter_id, result)
            });
        }
        let mut billing_error: Option<anyhow::Error> = None;
        while let Some((chapter_id, result)) = tasks.next().await {
            if let Err(error) = result {
                if error_is_billing(&error) {
                    state.halt.store(true, Ordering::SeqCst);
                    if billing_error.is_none() {
                        billing_error = Some(error);
                    }
                } else {
                    error!("章节 {chapter_id} 意外失败: {error:#}");
                }
            }
        }
        if let Some(error) = billing_error {
            return Err(error);
        }

        let stats = self.store.stats()?;
        info!(
            "翻译结束：QA_PASSED={} / HUMAN_REVIEW={} / QA_FAILED={}；累计 token={}+{}，成本约 ${:.4}",
            stats.states.get("QA_PASSED").copied().unwrap_or(0),
            stats.states.get("HUMAN_REVIEW").copied().unwrap_or(0),
            stats.states.get("QA_FAILED").copied().unwrap_or(0),
            stats.prompt_tokens,
            stats.completion_tokens,
            stats.cost_usd
        );
        Ok(stats)
    }

    async fn run_chapter(
        &self,
        chapter: &ChapterRecord,
        core_characters: &[Character],
        retry_human: bool,
        state: &RunState,
    ) -> Result<()> {
        let segments = self.store.list_segments(Some(&chapter.id))?;
        if segments.is_empty() {
            return Ok(());
        }
        info!(
            "▶ {} {}（{} 段）",
            chapter.id,
            chapter.title,
            segments.len()
        );

        let mut infos: Vec<(MatchedEntities, HashSet<String>)> = Vec::with_capacity(segments.len());
        let mut seen: HashSet<String> = HashSet::new();
        for segment in &segments {
            let mentioned = self.matcher.matched_entities(&segment.source_text);
            let keys = entity_keys(&mentioned);
            let first: HashSet<String> = keys.difference(&seen).cloned().collect();
            seen.extend(keys);
            infos.push((mentioned, first));
        }

        let mut prev_texts: Vec<String> = Vec::new();
        let mut processed = 0usize;
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;

        for (index, segment) in segments.iter().enumerate() {
            if state.halt.load(Ordering::Relaxed) {
                warn!("计费/额度保护已触发，章节 {} 提前停止", chapter.id);
                return Ok(());
            }
            if segment.state == SegmentState::QaPassed {
                skipped += 1;
                push_context(
                    &mut prev_texts,
                    segment
                        .final_text
                        .as_deref()
                        .or(segment.best_text.as_deref())
                        .unwrap_or(&segment.source_text),
                );
                continue;
            }
            if segment.state == SegmentState::HumanReview && !retry_human {
                skipped += 1;
                push_context(
                    &mut prev_texts,
                    segment
                        .best_text
                        .as_deref()
                        .or(segment.final_text.as_deref())
                        .unwrap_or(&segment.source_text),
                );
                continue;
            }
            if !state.take_slot() {
                info!("达到 --limit 上限，章节 {} 提前收尾", chapter.id);
                break;
            }

            let (mentioned, first_keys) = &infos[index];
            let prompt_entities = MatchedEntities {
                characters: core_characters.to_vec(),
                glossary: Vec::new(),
            }
            .merge(mentioned);
            let next_source = segments
                .get(index + 1)
                .map(|next| next.source_text.clone())
                .unwrap_or_else(|| "（无，本段位于本章结尾）".to_string());

            processed += 1;
            let text = self
                .process_segment(
                    segment,
                    &prev_texts,
                    &next_source,
                    mentioned,
                    &prompt_entities,
                    first_keys,
                    state,
                )
                .await?;
            match &text {
                Some(text) => push_context(&mut prev_texts, text),
                None => push_context(&mut prev_texts, &segment.source_text),
            }

            match self.store.get_segment(&segment.id)? {
                Some(latest) if latest.state == SegmentState::QaPassed => passed += 1,
                _ => failed += 1,
            }
        }

        info!(
            "✔ {}：处理 {} / 通过 {} / 未通过 {} / 跳过 {}",
            chapter.id, processed, passed, failed, skipped
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_segment(
        &self,
        segment: &SegmentRecord,
        prev_texts: &[String],
        next_source: &str,
        mentioned: &MatchedEntities,
        prompt_entities: &MatchedEntities,
        first_keys: &HashSet<String>,
        state: &RunState,
    ) -> Result<Option<String>> {
        let max_attempts = 1 + self.settings.qa.max_retry;
        let mut attempt = segment.retry_count.max(0) as usize;
        if attempt >= max_attempts {
            error!(
                "[{}] 历史重试次数已达上限（{}），转人工复核",
                segment.id, attempt
            );
            let text = segment.best_text.clone().or(segment.final_text.clone());
            self.mark_human(segment, "重试次数已耗尽", text.as_deref());
            return Ok(text);
        }

        let mut current = segment.clone();
        let mut best_text = current.best_text.clone().or(current.final_text.clone());
        let mut best_score = current.qa_score.unwrap_or(-1.0);
        let mut routing = current
            .last_routing
            .as_deref()
            .and_then(|raw| raw.parse::<ErrorRouting>().ok());
        let mut pass1_valid = current.pass1_draft.is_some();
        if routing == Some(ErrorRouting::RerunFromPass1) {
            pass1_valid = false;
        }
        let mut pending_translation = if current.state == SegmentState::Polished {
            current.final_text.clone()
        } else {
            None
        };
        let mut retry_feedback = current.last_error.clone().unwrap_or_default();
        let mut failed_translation = current.final_text.clone().unwrap_or_default();

        let result: Result<Option<String>> = async {
            while attempt < max_attempts {
                let translation = if let Some(pending) = pending_translation.take() {
                    pending
                } else {
                    if !pass1_valid {
                        let draft = self
                            .pass1(&current, prev_texts, next_source, mentioned)
                            .await?;
                        current = self.store.mark_drafted(
                            &current.id,
                            &draft.syntax_and_nuance_notes,
                            &draft.faithful_draft,
                        )?;
                        pass1_valid = true;
                    }
                    let polished = self
                        .pass2(
                            &current,
                            prev_texts,
                            prompt_entities,
                            &retry_feedback,
                            &failed_translation,
                        )
                        .await?;
                    let translation = polished.literary_translation;
                    current = self.store.mark_polished(&current.id, &translation)?;
                    translation
                };

                let qa = self
                    .qa
                    .evaluate(
                        &self.llm,
                        &current,
                        &translation,
                        mentioned,
                        prompt_entities,
                        first_keys,
                        prev_texts,
                    )
                    .await?;
                self.store.record_qa(
                    &current.id,
                    attempt as i64,
                    qa.passed,
                    qa.score,
                    qa.routing,
                    &qa.issues_json(),
                )?;

                if qa.passed {
                    self.store.mark_qa_passed(
                        &current.id,
                        &translation,
                        qa.score,
                        attempt as i64,
                    )?;
                    if let Some(reason) = &qa.judge_skipped_reason {
                        warn!("[QA] {} PASS（Judge 跳过：{}）", current.id, reason);
                    } else {
                        info!("[QA] {} PASS score={:?}", current.id, qa.score);
                    }
                    return Ok(Some(translation));
                }

                if let Some(score) = qa.score {
                    if score > best_score {
                        best_text = Some(translation.clone());
                        best_score = score;
                    }
                } else if best_text.is_none() {
                    best_text = Some(translation.clone());
                }

                retry_feedback = qa.feedback_text();
                failed_translation = translation;
                routing = Some(qa.routing);
                attempt += 1;

                current = self.store.mark_qa_failed(
                    &current.id,
                    best_text.as_deref().unwrap_or(&failed_translation),
                    if best_score >= 0.0 {
                        Some(best_score)
                    } else {
                        None
                    },
                    qa.routing,
                    if retry_feedback.is_empty() {
                        "未通过 QA"
                    } else {
                        &retry_feedback
                    },
                    attempt as i64,
                )?;

                warn!(
                    "[QA] {} FAIL attempt={} routing={:?}：{}",
                    current.id,
                    attempt - 1,
                    qa.routing,
                    retry_feedback.lines().next().unwrap_or("未知原因")
                );

                if attempt >= max_attempts {
                    let final_text = best_text
                        .clone()
                        .or(Some(failed_translation))
                        .unwrap_or_default();
                    self.store.mark_human(
                        &current.id,
                        Some(&final_text),
                        if retry_feedback.is_empty() {
                            "QA 未通过，重试耗尽"
                        } else {
                            &retry_feedback
                        },
                    )?;
                    error!("[QA] {} 重试耗尽，转人工复核（保留最优版本）", current.id);
                    return Ok(Some(final_text));
                }

                pass1_valid = qa.routing != ErrorRouting::RerunFromPass1;
            }
            Ok(best_text.or(current.final_text))
        }
        .await;

        match result {
            Ok(text) => Ok(text),
            Err(error) => {
                if error_is_billing(&error) {
                    // 欠费/额度耗尽：保留最近检查点，停止整轮，不降级为 HUMAN_REVIEW
                    error!("[{}] 计费/额度错误，停止本轮翻译: {error:#}", segment.id);
                    state.halt.store(true, Ordering::SeqCst);
                    return Err(error);
                }
                error!("[{}] 处理异常，转人工复核: {error:#}", segment.id);
                let fallback = self
                    .store
                    .get_segment(&segment.id)
                    .ok()
                    .flatten()
                    .and_then(|latest| latest.best_text.or(latest.final_text))
                    .or_else(|| segment.final_text.clone());
                self.mark_human(segment, &format!("{error:#}"), fallback.as_deref());
                Ok(fallback)
            }
        }
    }

    async fn pass1(
        &self,
        segment: &SegmentRecord,
        prev_texts: &[String],
        next_source: &str,
        mentioned: &MatchedEntities,
    ) -> Result<Pass1Result> {
        let user = self.prompts.render(
            "pass1_user.j2",
            json!({
                "active_glossary_json": compact_entities_json(mentioned),
                "prev_translations": join_paragraphs(prev_texts),
                "next_source": next_source,
                "current_source": segment.source_text,
            }),
        )?;
        self.llm
            .structured::<Pass1Result>(
                RouteName::Pass1,
                &self.pass1_system,
                &user,
                "pass1",
                json!({"source_text": segment.source_text}),
                Some(&segment.id),
            )
            .await
            .context("Pass 1 直译失败")
    }

    async fn pass2(
        &self,
        segment: &SegmentRecord,
        prev_texts: &[String],
        prompt_entities: &MatchedEntities,
        retry_feedback: &str,
        failed_translation: &str,
    ) -> Result<Pass2Result> {
        let draft = segment
            .pass1_draft
            .clone()
            .unwrap_or_else(|| segment.source_text.clone());
        let user = self.prompts.render(
            "pass2_user.j2",
            json!({
                "active_entities_json": entities_json(prompt_entities),
                "prev_translations": join_paragraphs(prev_texts),
                "current_source": segment.source_text,
                "pass1_notes": segment.pass1_notes.clone().unwrap_or_else(|| "（无）".to_string()),
                "pass1_draft": draft,
                "retry_feedback": retry_feedback,
                "previous_failed_translation": if failed_translation.is_empty() { "（无）".to_string() } else { failed_translation.to_string() },
            }),
        )?;
        self.llm
            .structured::<Pass2Result>(
                RouteName::Pass2,
                &self.pass2_system,
                &user,
                "pass2",
                json!({"source_text": segment.source_text, "draft": draft}),
                Some(&segment.id),
            )
            .await
            .context("Pass 2 文学重塑失败")
    }

    fn mark_human(&self, segment: &SegmentRecord, reason: &str, text: Option<&str>) {
        if let Err(error) = self
            .store
            .mark_human(&segment.id, text, &error_text(reason))
        {
            warn!("[{}] 标记 HUMAN_REVIEW 失败: {error:#}", segment.id);
        }
    }
}

fn error_text(text: &str) -> String {
    text.chars().take(2000).collect()
}

fn entity_keys(entities: &MatchedEntities) -> HashSet<String> {
    let mut keys: HashSet<String> = entities
        .characters
        .iter()
        .map(|character| format!("char:{}", character.canonical_id))
        .collect();
    keys.extend(
        entities
            .glossary
            .iter()
            .map(|entry| format!("gloss:{}", entry.source_term)),
    );
    keys
}

fn push_context(prev_texts: &mut Vec<String>, text: &str) {
    prev_texts.push(text.to_string());
    while prev_texts.len() > 2 {
        prev_texts.remove(0);
    }
}
