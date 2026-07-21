//! Smart model dispatch: suggest which model tier a prompt probably needs,
//! from a small keyword-weighted classifier plus a user-overridable
//! `dispatch.toml`.

use std::collections::HashMap;

use crate::{Error, Result};

/// Parsed `dispatch.toml`: `[rules] <category> = "<model>"` and
/// `[keywords] <category> = ["kw1", "kw2", ...]`. Any category or keyword
/// list missing from the input file falls back to the built-in default for
/// that category — the file only needs to override what it disagrees with.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchConfig {
    pub rules: HashMap<String, String>,
    pub keywords: HashMap<String, Vec<String>>,
}

/// The four categories not-goldfish classifies prompts into, from cheapest
/// to most expensive intent.
pub const CATEGORIES: &[&str] = &["quick_fix", "standard", "architecture", "research"];

impl DispatchConfig {
    /// Built-in defaults: sensible pt/en keyword coverage and a model tier
    /// per category, usable with no `dispatch.toml` at all.
    pub fn default_config() -> Self {
        let rules = [
            ("quick_fix", "haiku"),
            ("standard", "sonnet"),
            ("architecture", "opus"),
            ("research", "opus"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let keywords = [
            (
                "quick_fix",
                vec![
                    "fix", "bug", "typo", "corrigir", "conserta", "ajustar", "ajuste", "quick",
                    "rápido", "rapido", "pequeno", "rename", "renomear",
                ],
            ),
            (
                "standard",
                vec![
                    "implement",
                    "implementar",
                    "add",
                    "adicionar",
                    "feature",
                    "build",
                    "construir",
                    "refactor",
                    "refatorar",
                    "criar",
                    "create",
                    "update",
                    "atualizar",
                ],
            ),
            (
                "architecture",
                vec![
                    "architecture",
                    "arquitetura",
                    "design",
                    "redesign",
                    "migrate",
                    "migração",
                    "migracao",
                    "sistema",
                    "system",
                    "scale",
                    "escalar",
                    "infra",
                    "infraestrutura",
                ],
            ),
            (
                "research",
                vec![
                    "research",
                    "pesquisar",
                    "pesquisa",
                    "investigate",
                    "investigar",
                    "analyze",
                    "analisar",
                    "compare",
                    "comparar",
                    "avaliar",
                    "evaluate",
                    "avaliação",
                ],
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.into_iter().map(str::to_string).collect()))
        .collect();

        DispatchConfig { rules, keywords }
    }

    /// Parse a `dispatch.toml` document, layering it over
    /// [`DispatchConfig::default_config`]. A syntactically invalid document
    /// is a clear `Err`, never a silent fallback and never a panic — only
    /// missing sections/categories fall back to defaults.
    pub fn from_toml(input: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(input)?;
        let mut config = Self::default_config();

        if let Some(rules) = value.get("rules").and_then(|v| v.as_table()) {
            for (category, model) in rules {
                if let Some(model) = model.as_str() {
                    config.rules.insert(category.clone(), model.to_string());
                } else {
                    return Err(Error::Other(format!(
                        "dispatch.toml: rules.{category} deve ser uma string"
                    )));
                }
            }
        }

        if let Some(keywords) = value.get("keywords").and_then(|v| v.as_table()) {
            for (category, list) in keywords {
                let Some(array) = list.as_array() else {
                    return Err(Error::Other(format!(
                        "dispatch.toml: keywords.{category} deve ser uma lista"
                    )));
                };
                let mut parsed = Vec::with_capacity(array.len());
                for entry in array {
                    let Some(s) = entry.as_str() else {
                        return Err(Error::Other(format!(
                            "dispatch.toml: keywords.{category} deve conter só strings"
                        )));
                    };
                    parsed.push(s.to_string());
                }
                config.keywords.insert(category.clone(), parsed);
            }
        }

        Ok(config)
    }
}

/// A dispatch suggestion for one prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub category: String,
    pub model: String,
    pub matched: Vec<String>,
}

/// Classify `prompt` by keyword weight against every category in
/// `config.keywords`. Each matching keyword contributes
/// `word_count + char_count/20` to its category's score — multi-word
/// phrases are more specific than single words, and among single words,
/// longer ones are less likely to be a coincidental substring match. A
/// prompt that matches nothing, or ties between categories, resolves to
/// `"standard"` — the safe, unsurprising default tier.
pub fn suggest(config: &DispatchConfig, prompt: &str) -> Suggestion {
    let lower = prompt.to_lowercase();

    let mut scored: Vec<(&str, f64, Vec<String>)> = Vec::with_capacity(CATEGORIES.len());
    for &category in CATEGORIES {
        let mut score = 0.0;
        let mut matched = Vec::new();
        if let Some(keywords) = config.keywords.get(category) {
            for keyword in keywords {
                let keyword_lower = keyword.to_lowercase();
                if !keyword_lower.is_empty() && lower.contains(&keyword_lower) {
                    let weight = keyword.split_whitespace().count() as f64
                        + keyword.chars().count() as f64 / 20.0;
                    score += weight;
                    matched.push(keyword.clone());
                }
            }
        }
        scored.push((category, score, matched));
    }

    let max_score = scored
        .iter()
        .map(|(_, score, _)| *score)
        .fold(0.0, f64::max);
    let winners: Vec<&(&str, f64, Vec<String>)> = scored
        .iter()
        .filter(|(_, score, _)| *score == max_score)
        .collect();

    let (category, matched) = if max_score <= 0.0 || winners.len() > 1 {
        ("standard", Vec::new())
    } else {
        (winners[0].0, winners[0].2.clone())
    };

    let model = config
        .rules
        .get(category)
        .cloned()
        .unwrap_or_else(|| "sonnet".to_string());

    Suggestion {
        category: category.to_string(),
        model,
        matched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_covers_every_category() {
        let config = DispatchConfig::default_config();
        for category in CATEGORIES {
            assert!(config.rules.contains_key(*category));
            assert!(!config.keywords.get(*category).unwrap().is_empty());
        }
    }

    #[test]
    fn classifies_portuguese_quick_fix() {
        let config = DispatchConfig::default_config();
        let s = suggest(&config, "corrigir esse bug rápido no login");
        assert_eq!(s.category, "quick_fix");
        assert_eq!(s.model, "haiku");
        assert!(!s.matched.is_empty());
    }

    #[test]
    fn classifies_english_standard() {
        let config = DispatchConfig::default_config();
        let s = suggest(&config, "implement a new feature for the export API");
        assert_eq!(s.category, "standard");
        assert_eq!(s.model, "sonnet");
    }

    #[test]
    fn classifies_architecture() {
        let config = DispatchConfig::default_config();
        let s = suggest(
            &config,
            "precisamos redesenhar a arquitetura pra escalar o sistema",
        );
        assert_eq!(s.category, "architecture");
        assert_eq!(s.model, "opus");
    }

    #[test]
    fn classifies_research() {
        let config = DispatchConfig::default_config();
        let s = suggest(&config, "research and compare three caching libraries");
        assert_eq!(s.category, "research");
        assert_eq!(s.model, "opus");
    }

    #[test]
    fn no_match_falls_back_to_standard() {
        let config = DispatchConfig::default_config();
        let s = suggest(&config, "olá, tudo bem?");
        assert_eq!(s.category, "standard");
        assert!(s.matched.is_empty());
    }

    #[test]
    fn tie_falls_back_to_standard() {
        let mut config = DispatchConfig::default_config();
        config
            .keywords
            .insert("quick_fix".to_string(), vec!["xyz".to_string()]);
        config
            .keywords
            .insert("research".to_string(), vec!["abc".to_string()]);
        // Both single-word keywords of equal length -> equal score -> tie.
        let s = suggest(&config, "xyz and abc appear here");
        assert_eq!(s.category, "standard");
    }

    #[test]
    fn from_toml_overrides_only_given_categories() {
        let toml_str = r#"
            [rules]
            quick_fix = "haiku-fast"

            [keywords]
            quick_fix = ["hotfix"]
        "#;
        let config = DispatchConfig::from_toml(toml_str).unwrap();
        assert_eq!(config.rules["quick_fix"], "haiku-fast");
        assert_eq!(config.keywords["quick_fix"], vec!["hotfix".to_string()]);
        // Untouched categories keep the built-in defaults.
        assert_eq!(config.rules["standard"], "sonnet");
        assert!(config.keywords["research"].contains(&"pesquisar".to_string()));
    }

    #[test]
    fn from_toml_rejects_invalid_syntax_with_clear_error() {
        let err = DispatchConfig::from_toml("this is not [ valid toml");
        assert!(err.is_err());
    }

    #[test]
    fn from_toml_rejects_wrong_shaped_values() {
        let err = DispatchConfig::from_toml("[rules]\nquick_fix = 5\n");
        assert!(err.is_err());
        let err = DispatchConfig::from_toml("[keywords]\nquick_fix = \"not-a-list\"\n");
        assert!(err.is_err());
    }
}
