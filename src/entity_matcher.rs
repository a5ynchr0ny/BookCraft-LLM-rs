//! 实体检索与动态挂载（Aho-Corasick 别名匹配 + 核心角色频次排序）。

use std::collections::{BTreeMap, HashMap, HashSet};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde_json::{Value, json};

use crate::models::{Character, GlossaryEntry, TranslationBible};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityMatch {
    pub entity_key: String,
    pub alias: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default)]
pub struct MatchedEntities {
    pub characters: Vec<Character>,
    pub glossary: Vec<GlossaryEntry>,
}

impl MatchedEntities {
    pub fn is_empty(&self) -> bool {
        self.characters.is_empty() && self.glossary.is_empty()
    }

    pub fn to_payload(&self, include_voices: bool) -> Value {
        let characters: Vec<Value> = self
            .characters
            .iter()
            .map(|character| {
                let mut item = json!({
                    "canonical_id": character.canonical_id,
                    "source_name": character.source_name,
                    "aliases": character.aliases,
                    "target_name": character.target_name,
                    "short_target_map": character.short_target_map,
                    "forbidden_variants": character.forbidden_variants,
                    "identity_and_arc": character.identity_and_arc,
                });
                if include_voices {
                    if let Value::Object(map) = &mut item {
                        map.insert(
                            "voice_profile".into(),
                            Value::String(character.voice_profile.clone()),
                        );
                        map.insert(
                            "pronoun_matrix".into(),
                            serde_json::to_value(&character.pronoun_matrix).unwrap_or(Value::Null),
                        );
                    }
                }
                item
            })
            .collect();

        let glossary: Vec<Value> = self
            .glossary
            .iter()
            .map(|entry| {
                json!({
                    "source_term": entry.source_term,
                    "aliases": entry.aliases,
                    "target_term": entry.target_term,
                    "forbidden_variants": entry.forbidden_variants,
                    "category": entry.category,
                    "context_note": entry.context_note,
                })
            })
            .collect();

        json!({ "characters": characters, "glossary": glossary })
    }

    pub fn merge(&self, other: &MatchedEntities) -> MatchedEntities {
        let char_ids: HashSet<&str> = self
            .characters
            .iter()
            .map(|c| c.canonical_id.as_str())
            .collect();
        let glossary_terms: HashSet<&str> = self
            .glossary
            .iter()
            .map(|g| g.source_term.as_str())
            .collect();
        let mut characters = self.characters.clone();
        characters.extend(
            other
                .characters
                .iter()
                .filter(|c| !char_ids.contains(c.canonical_id.as_str()))
                .cloned(),
        );
        let mut glossary = self.glossary.clone();
        glossary.extend(
            other
                .glossary
                .iter()
                .filter(|g| !glossary_terms.contains(g.source_term.as_str()))
                .cloned(),
        );
        MatchedEntities {
            characters,
            glossary,
        }
    }
}

pub struct EntityMatcher {
    characters: HashMap<String, Character>,
    glossary: HashMap<String, GlossaryEntry>,
    automaton: Option<AhoCorasick>,
    patterns: Vec<String>,
    pattern_keys: Vec<Vec<String>>,
}

impl EntityMatcher {
    pub fn new(bible: &TranslationBible) -> Self {
        let mut characters = HashMap::new();
        let mut glossary = HashMap::new();
        let mut alias_to_keys: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for character in &bible.core_characters {
            let key = format!("char:{}", character.canonical_id);
            characters.insert(key.clone(), character.clone());
            for form in character.all_source_forms() {
                register_alias(&mut alias_to_keys, &form, &key);
            }
        }

        for (index, entry) in bible.glossary.iter().enumerate() {
            let entry_id = if entry.id.is_empty() {
                format!("gloss_{:04}", index + 1)
            } else {
                entry.id.clone()
            };
            let key = format!("gloss:{entry_id}");
            glossary.insert(key.clone(), entry.clone());
            for form in entry.all_source_forms() {
                register_alias(&mut alias_to_keys, &form, &key);
            }
        }

        let mut patterns = Vec::new();
        let mut pattern_keys = Vec::new();
        for (alias, keys) in alias_to_keys {
            patterns.push(alias);
            pattern_keys.push(keys);
        }

        let automaton = if patterns.is_empty() {
            None
        } else {
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .build(&patterns)
                .ok()
        };

        Self {
            characters,
            glossary,
            automaton,
            patterns,
            pattern_keys,
        }
    }

    pub fn matches(&self, text: &str) -> Vec<EntityMatch> {
        let Some(automaton) = &self.automaton else {
            return Vec::new();
        };
        let lowered = text.to_lowercase();
        if lowered.len() != text.len() {
            return Vec::new();
        }

        let mut candidates: Vec<(usize, usize, String)> = Vec::new();
        for found in automaton.find_overlapping_iter(&lowered) {
            let alias = &self.patterns[found.pattern().as_usize()];
            let start = found.start();
            let end = found.end();
            if !boundary_ok(&lowered, start, end, alias) {
                continue;
            }
            for key in &self.pattern_keys[found.pattern().as_usize()] {
                candidates.push((start, end, key.clone()));
            }
        }

        candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| (b.1 - b.0).cmp(&(a.1 - a.0))));
        let mut accepted = Vec::new();
        let mut last_end = 0usize;
        for (start, end, key) in candidates {
            if start < last_end {
                continue;
            }
            accepted.push(EntityMatch {
                entity_key: key,
                alias: lowered[start..end].to_string(),
                start,
                end,
            });
            last_end = end;
        }
        accepted
    }

    pub fn matched_entities(&self, text: &str) -> MatchedEntities {
        let mut characters = Vec::new();
        let mut glossary = Vec::new();
        let mut seen = HashSet::new();
        for entity_match in self.matches(text) {
            if !seen.insert(entity_match.entity_key.clone()) {
                continue;
            }
            if let Some(character) = self.characters.get(&entity_match.entity_key) {
                characters.push(character.clone());
            } else if let Some(entry) = self.glossary.get(&entity_match.entity_key) {
                glossary.push(entry.clone());
            }
        }
        MatchedEntities {
            characters,
            glossary,
        }
    }

    pub fn count_mentions(&self, texts: &[String]) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for text in texts {
            for entity_match in self.matches(text) {
                *counts.entry(entity_match.entity_key).or_insert(0) += 1;
            }
        }
        counts
    }

    pub fn core_characters(&self, texts: &[String], top_n: usize) -> Vec<Character> {
        let counts = self.count_mentions(texts);
        let mut ranked: Vec<(&String, &Character)> = self.characters.iter().collect();
        ranked.sort_by(|a, b| {
            let count_a = counts.get(a.0).copied().unwrap_or(0);
            let count_b = counts.get(b.0).copied().unwrap_or(0);
            count_b
                .cmp(&count_a)
                .then_with(|| a.1.canonical_id.cmp(&b.1.canonical_id))
        });
        ranked
            .into_iter()
            .take(top_n)
            .map(|(_, character)| character.clone())
            .collect()
    }
}

fn register_alias(index: &mut BTreeMap<String, Vec<String>>, form: &str, key: &str) {
    let normalized = form.trim().to_lowercase();
    if normalized.chars().count() < 2 || !normalized.chars().any(|ch| ch.is_alphanumeric()) {
        return;
    }
    let keys = index.entry(normalized).or_default();
    if !keys.iter().any(|existing| existing == key) {
        keys.push(key.to_string());
    }
}

fn boundary_ok(text: &str, start: usize, end: usize, alias: &str) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    if before.map(|ch| ch.is_alphanumeric()).unwrap_or(false) {
        return false;
    }
    if after.map(|ch| ch.is_alphanumeric()).unwrap_or(false) {
        return false;
    }
    if !alias.contains('-') && (before == Some('-') || after == Some('-')) {
        return false;
    }
    true
}
