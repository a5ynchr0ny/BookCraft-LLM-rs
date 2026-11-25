//! Prompt 模板渲染（minijinja）。

use std::path::Path;

use anyhow::{Context, Result};
use minijinja::syntax::SyntaxConfig;
use minijinja::value::Serde;
use minijinja::{Environment, UndefinedBehavior, path_loader};
use serde_json::Value;

use crate::entity_matcher::MatchedEntities;
use crate::models::TranslationBible;

pub struct PromptLibrary {
    env: Environment<'static>,
}

impl PromptLibrary {
    pub fn new(prompts_dir: impl AsRef<Path>) -> Result<Self> {
        let dir = prompts_dir.as_ref().to_path_buf();
        if !dir.exists() {
            anyhow::bail!("Prompt 目录不存在: {}", dir.display());
        }
        let mut env = Environment::new();
        env.set_loader(path_loader(dir));
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.set_syntax(
            SyntaxConfig::builder()
                .trim_blocks(true)
                .lstrip_blocks(true)
                .build()
                .map_err(|error| anyhow::anyhow!("Jinja 语法配置失败: {error}"))?,
        );
        Ok(Self { env })
    }

    pub fn render(&self, name: &str, context: Value) -> Result<String> {
        let template = self
            .env
            .get_template(name)
            .with_context(|| format!("加载 Prompt 模板失败: {name}"))?;
        let rendered = template
            .render(Serde(&context))
            .with_context(|| format!("渲染 Prompt 模板失败: {name}"))?;
        Ok(rendered.trim().to_string())
    }
}

pub fn style_guide_text(bible: &TranslationBible) -> String {
    let guide = &bible.global_style_guide;
    [
        format!(
            "- 叙事基调：{}",
            non_empty(&guide.narrative_tone, "（未指定）")
        ),
        format!(
            "- 自由间接引语处理：{}",
            non_empty(&guide.free_indirect_discourse_rule, "（未指定）")
        ),
        format!(
            "- 长难句与节奏：{}",
            non_empty(&guide.syntax_strategy, "（未指定）")
        ),
        format!(
            "- 称谓规范：{}",
            non_empty(&guide.honorifics_rules, "（未指定）")
        ),
    ]
    .join("\n")
}

fn non_empty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

pub fn entities_json(entities: &MatchedEntities) -> String {
    serde_json::to_string_pretty(&entities.to_payload(true)).unwrap_or_else(|_| "{}".into())
}

pub fn compact_entities_json(entities: &MatchedEntities) -> String {
    serde_json::to_string_pretty(&entities.to_payload(false)).unwrap_or_else(|_| "{}".into())
}

pub fn join_paragraphs(paragraphs: &[String]) -> String {
    if paragraphs.is_empty() {
        "（无，本段位于本章开头）".to_string()
    } else {
        paragraphs.join("\n\n")
    }
}
