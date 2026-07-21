//! Synchronous lexical tag extraction — must stay under ~1ms for typical
//! payloads because it runs inside the hook hot path. No ML here; semantic
//! enrichment happens later in the daemon's async workers (Phase 2).

use std::collections::HashMap;

/// Small bilingual (en + pt-BR) stopword list. Kept deliberately short:
/// false negatives are cheap (a stopword indexed as tag is harmless),
/// false positives are not.
pub(crate) const STOPWORDS: &[&str] = &[
    // en
    "the", "a", "an", "and", "or", "but", "if", "then", "else", "for", "of", "to", "in", "on", "at",
    "by", "with", "from", "as", "is", "are", "was", "were", "be", "been", "it", "its", "this",
    "that", "these", "those", "i", "you", "he", "she", "we", "they", "my", "your", "not", "no",
    "yes", "do", "does", "did", "have", "has", "had", "will", "would", "can", "could", "should",
    "there", "what", "which", "who", "how", "when", "where", "why", "all", "any", "some", "just",
    "also", "than", "then", // pt-BR
    "o", "os", "um", "uma", "uns", "umas", "e", "ou", "mas", "se", "para", "por", "de", "do", "da",
    "dos", "das", "em", "no", "na", "nos", "nas", "com", "sem", "que", "quem", "qual", "quais",
    "como", "quando", "onde", "porque", "isso", "isto", "aquilo", "ele", "ela", "eles", "elas",
    "eu", "voce", "nos", "meu", "minha", "seu", "sua", "nao", "sim", "ser", "estar", "foi", "era",
    "sao", "esta", "estao", "tem", "tinha", "vai", "pode", "deve", "tambem", "mais", "menos",
    "muito", "pouco", "todo", "toda", "todos", "todas", "ja", "ainda", "so", "apenas",
];

const MAX_TAGS: usize = 24;
const MIN_TOKEN_LEN: usize = 3;

/// Extract space-separated tags from free text: file paths, URLs and the
/// most frequent non-stopword terms (identifiers included).
pub fn extract_tags(text: &str) -> String {
    let mut tags: Vec<String> = Vec::new();
    let mut freq: HashMap<String, usize> = HashMap::new();

    for raw in text.split_whitespace() {
        // Preserve paths and URLs verbatim — they are the highest-signal tags.
        if (raw.contains('/') && raw.len() > MIN_TOKEN_LEN) || raw.starts_with("http") {
            let cleaned = raw.trim_matches(|c: char| "\"'`(),;:<>[]{}".contains(c));
            if cleaned.len() > MIN_TOKEN_LEN
                && tags.len() < MAX_TAGS
                && !tags.iter().any(|t| t == cleaned)
            {
                tags.push(cleaned.to_string());
            }
            continue;
        }
        let token: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
            .collect::<String>()
            .to_lowercase();
        let token = token.trim_matches(|c| c == '.' || c == '-').to_string();
        if token.len() < MIN_TOKEN_LEN || token.chars().all(|c| c.is_numeric()) {
            continue;
        }
        if STOPWORDS.contains(&token.as_str()) {
            continue;
        }
        *freq.entry(token).or_insert(0) += 1;
    }

    let mut ranked: Vec<(String, usize)> = freq.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (token, _) in ranked {
        if tags.len() >= MAX_TAGS {
            break;
        }
        if !tags.contains(&token) {
            tags.push(token);
        }
    }
    tags.join(" ")
}

/// Cheap "is this pasted code/JSON/SQL rather than prose?" heuristic:
/// density of structural symbols. Prose almost never exceeds ~4% of
/// `{}();=<>[]"`-class characters; code, JSON and SQL comfortably do.
/// Runs in O(len) with no allocation — safe for capture-adjacent paths.
pub fn is_mostly_code(text: &str) -> bool {
    let len = text.chars().count();
    if len < 40 {
        return false;
    }
    let structural = text
        .chars()
        .filter(|c| {
            matches!(
                c,
                '{' | '}' | '(' | ')' | ';' | '=' | '<' | '>' | '[' | ']' | '"'
            )
        })
        .count();
    (structural as f64) / (len as f64) > 0.04
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_paths_and_keywords() {
        let tags = extract_tags("Fix the auth bug in src/auth/login.rs quando o login falha");
        assert!(tags.contains("src/auth/login.rs"));
        assert!(tags.contains("auth"));
        assert!(tags.contains("login"));
        assert!(!tags.split(' ').any(|t| t == "the"));
        assert!(!tags.split(' ').any(|t| t == "quando"));
    }

    #[test]
    fn empty_input() {
        assert_eq!(extract_tags(""), "");
    }

    #[test]
    fn caps_tag_count() {
        let long: String = (0..200).map(|i| format!("palavra{i} ")).collect();
        let tags = extract_tags(&long);
        assert!(tags.split(' ').count() <= MAX_TAGS);
    }

    #[test]
    fn detects_mostly_code() {
        assert!(is_mostly_code(
            "SELECT id FROM events WHERE id > ?1; INSERT INTO x (a,b) VALUES (1,2);"
        ));
        assert!(is_mostly_code(
            r#"{"type":"user","uuid":"abc-123","payload":{"content":[{"type":"text"}]}}"#
        ));
        assert!(is_mostly_code(
            "fn main() { let x: Vec<i64> = vec![]; if x.is_empty() { return; } }"
        ));
        assert!(!is_mostly_code(
            "vamos usar rusqlite para o banco porque é bundled e não depende do sistema"
        ));
        assert!(!is_mostly_code("qual é o status do build agora?"));
    }
}
