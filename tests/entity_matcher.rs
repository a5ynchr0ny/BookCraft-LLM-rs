use std::collections::BTreeMap;

use bookcraft::entity_matcher::EntityMatcher;
use bookcraft::models::{Character, GlossaryEntry, TranslationBible};

fn bible() -> TranslationBible {
    TranslationBible {
        core_characters: vec![
            Character {
                canonical_id: "char_001".into(),
                source_name: "Frédéric Moreau".into(),
                aliases: vec!["Frédéric".into(), "Moreau".into(), "M. Moreau".into()],
                target_name: "弗雷德里克·莫罗".into(),
                short_target_map: BTreeMap::from([
                    ("Frédéric".into(), "弗雷德里克".into()),
                    ("Moreau".into(), "莫罗".into()),
                ]),
                forbidden_variants: vec!["莫洛".into()],
                identity_and_arc: String::new(),
                voice_profile: "优柔寡断的浪漫主义青年".into(),
                pronoun_matrix: Vec::new(),
            },
            Character {
                canonical_id: "char_002".into(),
                source_name: "Emma".into(),
                aliases: Vec::new(),
                target_name: "爱玛".into(),
                short_target_map: BTreeMap::new(),
                forbidden_variants: Vec::new(),
                identity_and_arc: String::new(),
                voice_profile: String::new(),
                pronoun_matrix: Vec::new(),
            },
        ],
        glossary: vec![
            GlossaryEntry {
                id: "gloss_0001".into(),
                source_term: "Rouen".into(),
                aliases: Vec::new(),
                target_term: "鲁昂".into(),
                forbidden_variants: Vec::new(),
                category: "LOCATION".into(),
                context_note: String::new(),
            },
            GlossaryEntry {
                id: "gloss_0002".into(),
                source_term: "Madame Arnoux".into(),
                aliases: vec!["Mme Arnoux".into()],
                target_term: "阿尔努夫人".into(),
                forbidden_variants: Vec::new(),
                category: "OTHER".into(),
                context_note: String::new(),
            },
        ],
        ..Default::default()
    }
}

#[test]
fn longest_match_wins() {
    let matcher = EntityMatcher::new(&bible());
    let matches = matcher.matches("Frédéric Moreau arrive à Rouen.");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].entity_key, "char:char_001");
    assert_eq!(matches[0].alias, "frédéric moreau");
    assert_eq!(matches[1].entity_key, "gloss:gloss_0001");
}

#[test]
fn case_insensitive_matching() {
    let matcher = EntityMatcher::new(&bible());
    let matches = matcher.matches("FRÉDÉRIC part demain.");
    assert_eq!(matches[0].entity_key, "char:char_001");
}

#[test]
fn word_boundaries_and_hyphens() {
    let matcher = EntityMatcher::new(&bible());
    assert!(matcher.matches("Moreaumont est loin.").is_empty());
    assert!(matcher.matches("Moreau-Leroy est loin.").is_empty());
}

#[test]
fn hyphenated_entity_matches_when_alias_has_hyphen() {
    let bible = TranslationBible {
        glossary: vec![GlossaryEntry {
            id: "g1".into(),
            source_term: "Saint-Antoine".into(),
            aliases: Vec::new(),
            target_term: "圣安托万".into(),
            forbidden_variants: Vec::new(),
            category: "LOCATION".into(),
            context_note: String::new(),
        }],
        ..Default::default()
    };
    let matcher = EntityMatcher::new(&bible);
    assert_eq!(matcher.matches("Rue Saint-Antoine").len(), 1);
}

#[test]
fn aliases_and_glossary_resolution() {
    let matcher = EntityMatcher::new(&bible());
    let entities = matcher.matched_entities("Mme Arnoux parle à Moreau.");
    assert_eq!(entities.characters.len(), 1);
    assert_eq!(entities.characters[0].canonical_id, "char_001");
    assert_eq!(entities.glossary.len(), 1);
    assert_eq!(entities.glossary[0].source_term, "Madame Arnoux");
}

#[test]
fn core_characters_ranked_by_mentions() {
    let matcher = EntityMatcher::new(&bible());
    let texts = vec![
        "Emma".to_string(),
        "Emma".to_string(),
        "Frédéric Moreau".to_string(),
        "Rouen".to_string(),
    ];
    let ranked = matcher.core_characters(&texts, 2);
    assert_eq!(ranked[0].canonical_id, "char_002");
    assert_eq!(ranked[1].canonical_id, "char_001");
}

#[test]
fn empty_bible_is_safe() {
    let matcher = EntityMatcher::new(&TranslationBible::default());
    assert!(matcher.matches("anything").is_empty());
    assert!(
        matcher
            .core_characters(&["anything".to_string()], 5)
            .is_empty()
    );
}

#[test]
fn payload_hides_voice_when_not_requested() {
    let matcher = EntityMatcher::new(&bible());
    let entities = matcher.matched_entities("Moreau");
    let with_voice = entities.to_payload(true);
    let without_voice = entities.to_payload(false);
    assert!(with_voice["characters"][0].get("voice_profile").is_some());
    assert!(
        without_voice["characters"][0]
            .get("voice_profile")
            .is_none()
    );
}
