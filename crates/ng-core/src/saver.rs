//! Trait `Saver`: compressores externos plugáveis ("savers", plano 004).
//!
//! O core define só o contrato e os tipos; os transports concretos
//! (subprocess/CLI e MCP stdio, ambos em `ng-adapters`) ficam fora,
//! espelhando o padrão do trait [`crate::Embedder`].
//!
//! Invariantes do contrato (não-negociáveis, herdadas da higiene):
//! - Um saver só produz stub RECUPERÁVEL. O original permanece SEMPRE no
//!   banco not-goldfish; o `reversible_ref` é um *segundo* caminho de
//!   recuperação, nunca o único. Nada neste módulo (nem em nenhum call site)
//!   deleta ou substitui o conteúdo capturado — savers escrevem apenas
//!   colunas derivadas aditivas (`saved_digest`/`saved_ref`/`saved_by`).
//! - `tokens_before`/`tokens_after` são SEMPRE calculados pela nossa
//!   heurística ×4 bytes ([`estimate_tokens`]), nunca aceitos do saver — a
//!   lição do RTK (`docs/research/tooling-gains.md`): a economia
//!   auto-reportada de uma ferramenta é uma alegação sobre o contrafactual
//!   dela, não sobre a nossa conta. [`Compressed`] é `#[non_exhaustive]` de
//!   propósito: fora deste crate só se constrói via
//!   [`Compressed::from_input`], que computa os tokens ele mesmo.
//! - Savers NUNCA rodam em `ng-hook` (orçamento <5ms): só no worker
//!   background do `ngd` e no `ng saver bench`.

use crate::{Error, Result};

/// Heurística de tokens compartilhada (~4 bytes/token), a mesma de
/// [`crate::Event::tokens_est`]. Única fonte de contagem para savers.
pub fn estimate_tokens(text: &str) -> i64 {
    (text.len() / 4) as i64
}

/// Referência opaca de recuperação: `"<saver>:<chave>"` (ex.:
/// `"headroom:abc123"`). Serializável em texto puro para caber dentro do
/// stub no transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaverRef {
    pub saver: String,
    pub key: String,
}

impl SaverRef {
    /// Valida e constrói uma ref. A chave vai literalmente para dentro de
    /// um stub de transcript, então é dado não-confiável vindo do saver:
    /// vazia, gigante ou com whitespace/controle é recusada aqui, não
    /// sanitizada silenciosamente.
    pub fn new(saver: &str, key: &str) -> Result<Self> {
        if saver.is_empty() || key.is_empty() {
            return Err(Error::Other(
                "SaverRef: saver e chave não podem ser vazios".into(),
            ));
        }
        if key.len() > 256 {
            return Err(Error::Other("SaverRef: chave acima de 256 bytes".into()));
        }
        if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::Other(
                "SaverRef: chave não pode conter whitespace/controle".into(),
            ));
        }
        Ok(Self {
            saver: saver.to_string(),
            key: key.to_string(),
        })
    }

    /// Parse do formato textual `"<saver>:<chave>"` (inverso de `Display`).
    pub fn parse(text: &str) -> Result<Self> {
        let (saver, key) = text.split_once(':').ok_or_else(|| {
            Error::Other(format!("SaverRef inválida (esperado saver:chave): {text}"))
        })?;
        Self::new(saver, key)
    }
}

impl std::fmt::Display for SaverRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.saver, self.key)
    }
}

/// Resultado de uma compressão. `tokens_*` são SEMPRE nossos (×4 bytes) —
/// ver a doc do módulo. `#[non_exhaustive]` impede construção literal fora
/// do crate: todo transport passa por [`Compressed::from_input`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Compressed {
    pub text: String,
    pub tokens_before: i64,
    pub tokens_after: i64,
    /// Presente ⇒ o saver garante `retrieve(ref) == input` original.
    /// Ausente ⇒ o texto é um digest; a recuperação é pelo banco ng
    /// (que sempre existe — o original nunca sai de lá).
    pub reversible_ref: Option<SaverRef>,
}

impl Compressed {
    /// Único construtor: computa `tokens_before`/`tokens_after` pela nossa
    /// heurística e impõe o budget — saída acima do budget (ou maior que o
    /// próprio input) é ERRO, não "quase"; o chamador então usa o conteúdo
    /// original inalterado (pass-through).
    pub fn from_input(
        input: &str,
        text: String,
        budget: i64,
        reversible_ref: Option<SaverRef>,
    ) -> Result<Self> {
        let tokens_before = estimate_tokens(input);
        let tokens_after = estimate_tokens(&text);
        if tokens_after > budget {
            return Err(Error::Other(format!(
                "saver estourou o budget: ~{tokens_after} tokens > budget {budget}"
            )));
        }
        if tokens_after > tokens_before {
            return Err(Error::Other(
                "saver produziu digest maior que o original".into(),
            ));
        }
        Ok(Self {
            text,
            tokens_before,
            tokens_after,
            reversible_ref,
        })
    }
}

/// Compressor externo plugável. Contrato:
/// - `compress` respeita `budget` (tokens estimados ×4 bytes): saída maior
///   que o budget é ERRO, não "quase". Erro/timeout ⇒ chamador usa o
///   conteúdo original inalterado (pass-through; nada nunca quebra por
///   causa de um saver).
/// - `retrieve` devolve o original byte-a-byte para refs emitidas por este
///   saver.
/// - Implementações NÃO são chamadas em `ng-hook`; só no worker do `ngd` e
///   no bench.
pub trait Saver: Send + Sync {
    fn name(&self) -> &str;
    fn compress(&self, input: &str, budget: i64) -> Result<Compressed>;
    fn retrieve(&self, r: &SaverRef) -> Result<String>;
}

/// Saída de saver é dado, não instrução: colapsa caracteres de controle
/// (exceto `\n`/`\t`) em espaço e corta em `max_bytes` num limite de char,
/// com marcador explícito — o mesmo tratamento que o preview do
/// `[ng-evicted:]` recebe antes de entrar num stub de transcript.
pub fn sanitize_digest(text: &str, max_bytes: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    if cleaned.len() <= max_bytes {
        return cleaned;
    }
    let mut cut = max_bytes;
    while !cleaned.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut capped = cleaned;
    capped.truncate(cut);
    capped.push_str("[…]");
    capped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_len_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    #[test]
    fn from_input_computes_tokens_with_our_heuristic_never_the_savers() {
        // 40 bytes de input, 8 de saída: 10 → 2 tokens, venha o que vier
        // do saver externo (que nem tem como opinar — não há parâmetro).
        let input = "x".repeat(40);
        let c = Compressed::from_input(&input, "y".repeat(8), 100, None).unwrap();
        assert_eq!(c.tokens_before, 10);
        assert_eq!(c.tokens_after, 2);
    }

    #[test]
    fn from_input_rejects_output_over_budget() {
        let input = "x".repeat(400);
        let err = Compressed::from_input(&input, "y".repeat(200), 10, None);
        assert!(err.is_err(), "50 tokens de saída > budget 10 deve falhar");
    }

    #[test]
    fn from_input_rejects_digest_larger_than_original() {
        let err = Compressed::from_input(
            "curto",
            "muito mais longo que o original".into(),
            1000,
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn from_input_without_ref_still_recoverable_via_db() {
        // A garantia de recuperabilidade não depende da ref: ela é a
        // *segunda* via. Sem ref, o digest continua válido porque o
        // original nunca sai do banco (colunas saved_* são aditivas).
        let c =
            Compressed::from_input("original grande aqui...", "digest".into(), 100, None).unwrap();
        assert!(c.reversible_ref.is_none());
        assert_eq!(c.text, "digest");
    }

    #[test]
    fn saver_ref_roundtrips_display_and_parse() {
        let r = SaverRef::new("headroom", "abc123").unwrap();
        assert_eq!(r.to_string(), "headroom:abc123");
        assert_eq!(SaverRef::parse("headroom:abc123").unwrap(), r);
    }

    #[test]
    fn saver_ref_rejects_hostile_keys() {
        assert!(SaverRef::new("s", "").is_err());
        assert!(SaverRef::new("s", "com espaço").is_err());
        assert!(SaverRef::new("s", "com\nnewline").is_err());
        assert!(SaverRef::new("s", &"k".repeat(300)).is_err());
    }

    #[test]
    fn sanitize_digest_strips_control_and_caps() {
        let dirty = "a\u{1b}[31mb\x00c\nd";
        let clean = sanitize_digest(dirty, 1024);
        assert_eq!(clean, "a [31mb c\nd");

        let long = "é".repeat(600);
        let capped = sanitize_digest(&long, 100);
        assert!(capped.len() <= 100 + "[…]".len());
        assert!(capped.ends_with("[…]"));
    }
}
