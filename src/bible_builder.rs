//! Step 1：全书扫描生成《翻译圣经》。

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::info;

use crate::config::{RouteName, Settings};
use crate::llm::LlmService;
use crate::models::{TranslationBible, dedupe};
use crate::prompts::PromptLibrary;
use crate::state::{ChapterRecord, SegmentRecord, StateStore};

pub fn assemble_manuscript(
    chapters: &[ChapterRecord],
    segments_by_chapter: &std::collections::HashMap<String, Vec<SegmentRecord>>,
    max_chars: usize,
) -> (String, bool) {
    let mut chapter_texts: Vec<(String, String)> = Vec::new();
    for chapter in chapters {
        let body = segments_by_chapter
            .get(&chapter.id)
            .map(|segments| {
                segments
                    .iter()
                    .map(|segment| segment.source_text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();
        chapter_texts.push((chapter.title.clone(), body));
    }

    let total: usize = chapter_texts
        .iter()
        .map(|(title, body)| title.chars().count() + body.chars().count())
        .sum();
    let truncated = total > max_chars;
    let budget = if truncated {
        (max_chars / chapter_texts.len().max(1)).max(4000)
    } else {
        max_chars
    };

    let mut blocks = Vec::new();
    for (title, body) in chapter_texts {
        let excerpt = if body.chars().count() > budget {
            let chars: Vec<char> = body.chars().collect();
            let head_len = budget * 3 / 4;
            let tail_len = budget.saturating_sub(head_len);
            let head: String = chars.iter().take(head_len).collect();
            let tail: String = chars
                .iter()
                .skip(chars.len().saturating_sub(tail_len))
                .collect();
            format!("{head}\n……（本章中段省略，仅保留首尾以控制输入长度）……\n{tail}")
        } else {
            body
        };
        blocks.push(format!(
            "==================== {title} ====================\n{excerpt}"
        ));
    }

    (blocks.join("\n\n"), truncated)
}

pub struct BibleBuilder {
    settings: Arc<Settings>,
    store: Arc<StateStore>,
    llm: Arc<LlmService>,
    prompts: Arc<PromptLibrary>,
}

impl BibleBuilder {
    pub fn new(
        settings: Arc<Settings>,
        store: Arc<StateStore>,
        llm: Arc<LlmService>,
        prompts: Arc<PromptLibrary>,
    ) -> Self {
        Self {
            settings,
            store,
            llm,
            prompts,
        }
    }

    pub async fn build(&self, force: bool) -> Result<TranslationBible> {
        let meta = self.store.load_book_meta()?.ok_or_else(|| {
            anyhow::anyhow!("尚未解析书籍，请先运行：bookcraft prepare <book.epub>")
        })?;

        if !force {
            if let Some(existing) = self.store.load_bible()? {
                info!(
                    "复用已有《翻译圣经》：{} 个角色 / {} 条术语（--force 可重建）",
                    existing.core_characters.len(),
                    existing.glossary.len()
                );
                return Ok(existing);
            }
        }

        let chapters = self.store.list_chapters()?;
        let mut segments_by_chapter = std::collections::HashMap::new();
        for chapter in &chapters {
            segments_by_chapter.insert(
                chapter.id.clone(),
                self.store.list_segments(Some(&chapter.id))?,
            );
        }
        let (manuscript, truncated) = assemble_manuscript(
            &chapters,
            &segments_by_chapter,
            self.settings.concurrency.bible_max_input_chars,
        );
        info!(
            "扫描手稿 {} 字符（{} 章{}）",
            manuscript.chars().count(),
            chapters.len(),
            if truncated { "，已按章抽样" } else { "" }
        );

        let system = self.prompts.render("bible_system.j2", json!({}))?;
        let user = self.prompts.render(
            "bible_user.j2",
            json!({
                "book_title": meta.title,
                "author": if meta.author.is_empty() { "（未知）" } else { &meta.author },
                "source_lang": meta.source_lang,
                "full_book_text": manuscript,
            }),
        )?;

        let bible = self
            .llm
            .structured::<TranslationBible>(
                RouteName::Bible,
                &system,
                &user,
                "bible",
                json!({}),
                None,
            )
            .await
            .context("生成《翻译圣经》失败")?;
        let bible = normalize_bible(bible);
        self.store.save_bible(&bible)?;
        let path = self.settings.bible_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&bible)?)?;

        info!(
            "《翻译圣经》生成完成：{} 个核心角色 / {} 条术语",
            bible.core_characters.len(),
            bible.glossary.len()
        );
        Ok(bible)
    }
}

pub fn normalize_bible(mut bible: TranslationBible) -> TranslationBible {
    let mut used_ids: HashSet<String> = HashSet::new();
    for (index, character) in bible.core_characters.iter_mut().enumerate() {
        let mut canonical = character.canonical_id.trim().to_string();
        if canonical.is_empty() {
            canonical = format!("char_{:03}", index + 1);
        }
        while used_ids.contains(&canonical) {
            canonical.push_str("_x");
        }
        used_ids.insert(canonical.clone());
        character.canonical_id = canonical;
        character.source_name = character.source_name.trim().to_string();
        character.target_name = character.target_name.trim().to_string();
        character.aliases = dedupe(
            character
                .aliases
                .iter()
                .map(|alias| alias.trim().to_string())
                .filter(|alias| !alias.is_empty() && alias != &character.source_name)
                .collect(),
        );
        character.short_target_map = character
            .short_target_map
            .iter()
            .filter(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty())
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .collect();
        let allowed: HashSet<String> = std::iter::once(character.target_name.clone())
            .chain(character.short_target_map.values().cloned())
            .collect();
        character.forbidden_variants = dedupe(
            character
                .forbidden_variants
                .iter()
                .map(|variant| variant.trim().to_string())
                .filter(|variant| !variant.is_empty() && !allowed.contains(variant))
                .collect(),
        );
    }

    let mut used_glossary: HashSet<String> = HashSet::new();
    for (index, entry) in bible.glossary.iter_mut().enumerate() {
        let mut entry_id = entry.id.trim().to_string();
        if entry_id.is_empty() {
            entry_id = format!("gloss_{:04}", index + 1);
        }
        while used_glossary.contains(&entry_id) {
            entry_id.push_str("_x");
        }
        used_glossary.insert(entry_id.clone());
        entry.id = entry_id;
        entry.source_term = entry.source_term.trim().to_string();
        entry.target_term = entry.target_term.trim().to_string();
        entry.aliases = dedupe(
            entry
                .aliases
                .iter()
                .map(|alias| alias.trim().to_string())
                .filter(|alias| !alias.is_empty() && alias != &entry.source_term)
                .collect(),
        );
        entry.forbidden_variants = dedupe(
            entry
                .forbidden_variants
                .iter()
                .map(|variant| variant.trim().to_string())
                .filter(|variant| !variant.is_empty() && variant != &entry.target_term)
                .collect(),
        );
    }
    bible
}
