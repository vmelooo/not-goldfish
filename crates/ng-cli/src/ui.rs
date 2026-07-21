//! ui: camada mínima de estilo de terminal, sem nenhuma dependência nova.
//!
//! Cor só quando faz sentido: stdout é um terminal, `NO_COLOR` ausente
//! (<https://no-color.org>) e `CLICOLOR` não é `0`. `CLICOLOR_FORCE` (≠ 0) ou
//! `NG_COLOR=always` forçam cor mesmo fora de TTY; `NG_COLOR=never` força a
//! ausência. Saída em pipe é sempre UTF-8 puro, byte a byte — scripts e o
//! e2e dependem disso.
//!
//! Linguagem visual neobrutalista, espelhada na web UI: dourado da marca
//! (amarelo bold) para títulos e destaques, violeta (ANSI 256) para o
//! secundário — números, memória, grafo —, muted (bright black) para réguas
//! e rodapés, verde ok, amarelo aviso, vermelho bold erro. Hierarquia:
//! banner pesado (`━`) → seções com régua (`─`) → linhas kv alinhadas.
//! Teal (ciano) permanece para comandos ainda não migrados. Vocabulário de
//! glifos: `✓ ! ✗ ● → ✎ · — × …` mais blocos de barra. Sem emoji.

use std::fmt::Display;
use std::io::IsTerminal;

// Códigos SGR da paleta. Públicos para usos pontuais (ex.: glifos do doctor).
pub const GOLD: &str = "33;1"; // dourado da marca: amarelo + bold
pub const TEAL: &str = "36"; // legado: comandos ainda não migrados
/// Violeta secundário do design system. A base-16 só tem magenta (35), que
/// rende rosa na maioria dos temas; o índice 135 do ANSI 256 (#af5fff) é o
/// mais próximo do violeta da web. Pipes/NO_COLOR nunca veem este código.
pub const VIOLET: &str = "38;5;135";
pub const MUTED: &str = "90"; // bright black: labels, regras, rodapés
pub const OK: &str = "32";
pub const WARN: &str = "33";
pub const ERR: &str = "31;1";
pub const BOLD: &str = "1";
pub const DIM: &str = "2";

/// Largura total da linha de seção (`  título ─────…`) — fixa, sem consultar
/// o tamanho do terminal (degrada bem em qualquer largura ≥ ~40).
const RULE_WIDTH: usize = 72;
/// Coluna do label nas linhas `kv` (após a indentação de 4 espaços).
const LABEL_WIDTH: usize = 24;
/// Coluna dos valores numéricos alinhados à direita.
const NUM_WIDTH: usize = 8;

/// A paleta de um comando. `detect()` resolve cor uma vez por execução; nos
/// testes, o construtor literal (`Palette { enabled: false }`) fixa o modo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    enabled: bool,
}

impl Palette {
    /// Cor conforme ambiente: TTY + NO_COLOR/CLICOLOR, com overrides de força.
    pub fn detect() -> Palette {
        Palette {
            enabled: color_enabled(),
        }
    }

    /// Envolve `text` num par SGR quando a cor está ligada; caso contrário
    /// devolve o texto intacto (invariante: pipe ⇒ zero bytes ESC).
    pub fn paint(&self, code: &str, text: impl Display) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn gold(&self, text: impl Display) -> String {
        self.paint(GOLD, text)
    }
    pub fn teal(&self, text: impl Display) -> String {
        self.paint(TEAL, text)
    }
    pub fn violet(&self, text: impl Display) -> String {
        self.paint(VIOLET, text)
    }
    pub fn muted(&self, text: impl Display) -> String {
        self.paint(MUTED, text)
    }
    pub fn ok(&self, text: impl Display) -> String {
        self.paint(OK, text)
    }
    pub fn warn(&self, text: impl Display) -> String {
        self.paint(WARN, text)
    }
    pub fn err(&self, text: impl Display) -> String {
        self.paint(ERR, text)
    }
    pub fn bold(&self, text: impl Display) -> String {
        self.paint(BOLD, text)
    }
    pub fn dim(&self, text: impl Display) -> String {
        self.paint(DIM, text)
    }

    // ------------------------------------------------------------------
    // glifos de estado
    // ------------------------------------------------------------------

    /// `✓` verde: check passou / serviço de pé.
    pub fn ok_glyph(&self) -> String {
        self.paint(OK, "✓")
    }
    /// `!` amarelo: aviso, nunca derruba o processo.
    pub fn warn_glyph(&self) -> String {
        self.paint(WARN, "!")
    }
    /// `✗` vermelho bold: falha dura.
    pub fn err_glyph(&self) -> String {
        self.paint(ERR, "✗")
    }

    // ------------------------------------------------------------------
    // layout
    // ------------------------------------------------------------------

    /// Banner do comando: bloco neobrutalista de três linhas — régua grossa
    /// `━` em dourado, título em MAIÚSCULAS dourado, régua de fechamento.
    /// `note` (escopo) segue o título em dim preservando a caixa, porque
    /// costuma carregar paths; vazia, some junto com o espaço.
    pub fn banner(&self, title: &str, note: &str) -> String {
        let rule = self.gold("━".repeat(RULE_WIDTH));
        let title = self.gold(title.to_uppercase());
        let heading = if note.is_empty() {
            format!("  {title}")
        } else {
            format!("  {title} {}", self.dim(note))
        };
        format!("{rule}\n{heading}\n{rule}")
    }

    /// Cabeçalho de seção: `  TÍTULO ────────────` — título em MAIÚSCULAS
    /// bold, régua muted. A régua preenche até [`RULE_WIDTH`] colunas
    /// visíveis (conta por `char`, nunca por byte — os títulos são pt-BR).
    pub fn section(&self, title: &str) -> String {
        let head = title.to_uppercase();
        let fill = RULE_WIDTH.saturating_sub(head.chars().count() + 3).max(3);
        format!("  {} {}", self.bold(head), self.muted("─".repeat(fill)))
    }

    /// Linha label/valor: indentação 4, label na coluna [`LABEL_WIDTH`],
    /// valor como veio (já pintado/alinhado pelo chamador, se quiser).
    pub fn kv(&self, label: &str, value: impl Display) -> String {
        format!("    {label:<LABEL_WIDTH$}{value}")
    }

    /// `kv` com valor numérico à direita em [`NUM_WIDTH`] colunas, em violeta.
    pub fn kvn(&self, label: &str, value: impl Display) -> String {
        self.kv(label, self.right(VIOLET, value, NUM_WIDTH))
    }

    /// Alinha `value` à direita em `width` colunas e pinta com `code`. A cor
    /// é de primeiro plano, então os espaços de preenchimento dentro do span
    /// são invisíveis — alinhar antes de pintar é o que mantém a coluna
    /// certa mesmo com códigos ANSI no meio.
    pub fn right(&self, code: &str, value: impl Display, width: usize) -> String {
        self.paint(code, format!("{value:>width$}"))
    }

    /// Barra fina de blocos: parte cheia em violeta, vazia em muted.
    /// `fraction` é travada em 0..=1. Largura em colunas (1 glifo = 1 coluna).
    pub fn bar(&self, fraction: f64, width: usize) -> String {
        let f = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled = ((f * width as f64).round() as usize).min(width);
        let full = "█".repeat(filled);
        let empty = "░".repeat(width - filled);
        if self.enabled {
            format!("{}{}", self.paint(VIOLET, full), self.paint(MUTED, empty))
        } else {
            format!("{full}{empty}")
        }
    }
}

/// Resolve a política de cor a partir do ambiente real.
fn color_enabled() -> bool {
    resolve(
        std::env::var("NG_COLOR").ok().as_deref(),
        std::env::var("CLICOLOR_FORCE").ok().as_deref(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("CLICOLOR").ok().as_deref(),
        std::io::stdout().is_terminal(),
    )
}

/// Núcleo puro da política (testável sem tocar em env nem TTY):
/// forças vencem tudo; depois os desligadores; por fim, só TTY.
fn resolve(
    ng_color: Option<&str>,
    clicolor_force: Option<&str>,
    no_color: bool,
    clicolor: Option<&str>,
    is_tty: bool,
) -> bool {
    match ng_color {
        Some("always") => return true,
        Some("never") => return false,
        _ => {}
    }
    if clicolor_force.is_some_and(|v| v != "0") {
        return true;
    }
    if no_color || clicolor == Some("0") {
        return false;
    }
    is_tty
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sem cor nunca — o que um pipe vê.
    fn plain() -> Palette {
        Palette { enabled: false }
    }

    /// Com cor sempre — renderização ANSI.
    fn ansi() -> Palette {
        Palette { enabled: true }
    }

    #[test]
    fn plain_palette_returns_text_unchanged() {
        let p = plain();
        assert!(!p.enabled);
        assert_eq!(p.gold("oi"), "oi");
        assert_eq!(p.teal(42), "42");
        assert_eq!(p.violet("v"), "v");
        assert_eq!(p.muted("x"), "x");
        assert_eq!(p.ok("✓"), "✓");
        assert_eq!(p.warn("!"), "!");
        assert_eq!(p.err("✗"), "✗");
        assert_eq!(p.bold("b"), "b");
        assert_eq!(p.dim("d"), "d");
        assert_eq!(p.paint(GOLD, "sem cor"), "sem cor");
    }

    #[test]
    fn ansi_palette_wraps_with_sgr_pairs() {
        let p = ansi();
        assert!(p.enabled);
        assert_eq!(p.gold("oi"), "\x1b[33;1moi\x1b[0m");
        assert_eq!(p.violet("v"), "\x1b[38;5;135mv\x1b[0m");
        assert_eq!(p.err("✗"), "\x1b[31;1m✗\x1b[0m");
        assert_eq!(p.muted("x"), "\x1b[90mx\x1b[0m");
    }

    #[test]
    fn banner_is_a_heavy_block_with_uppercase_title() {
        let p = plain();
        let rule = "━".repeat(RULE_WIDTH);
        assert_eq!(
            p.banner("not-goldfish · ganho acumulado", "— global"),
            format!("{rule}\n  NOT-GOLDFISH · GANHO ACUMULADO — global\n{rule}")
        );
        // Nota vazia: só o título entre as réguas, sem espaço sobrando.
        assert_eq!(p.banner("título", ""), format!("{rule}\n  TÍTULO\n{rule}"));
    }

    #[test]
    fn banner_paints_rules_and_title_gold_and_note_dim() {
        let out = ansi().banner("status", "— x");
        assert!(out.starts_with("\x1b[33;1m━"), "out: {out:?}");
        assert!(out.contains("\x1b[33;1mSTATUS\x1b[0m"), "out: {out:?}");
        assert!(out.contains("\x1b[2m— x\x1b[0m"), "out: {out:?}");
    }

    #[test]
    fn section_uppercases_title_and_fills_rule_to_fixed_width() {
        let p = plain();
        let line = p.section("memória");
        assert!(line.starts_with("  MEMÓRIA "), "line: {line:?}");
        assert!(line.contains('─'));
        assert_eq!(line.chars().count(), RULE_WIDTH);
        // Título longo demais: régua mínima de 3, sem pânico.
        let long = p.section(&"x".repeat(RULE_WIDTH));
        assert!(long.ends_with("───"));
    }

    #[test]
    fn status_glyphs_are_check_bang_cross_in_status_colors() {
        assert_eq!(plain().ok_glyph(), "✓");
        assert_eq!(plain().warn_glyph(), "!");
        assert_eq!(plain().err_glyph(), "✗");
        assert_eq!(ansi().ok_glyph(), "\x1b[32m✓\x1b[0m");
        assert_eq!(ansi().warn_glyph(), "\x1b[33m!\x1b[0m");
        assert_eq!(ansi().err_glyph(), "\x1b[31;1m✗\x1b[0m");
    }

    /// NO_COLOR/pipe: o bloco inteiro (banner → seção → kv) sai sem um único
    /// byte ESC e com as colunas do kv intactas.
    #[test]
    fn plain_composed_block_has_zero_escape_bytes_and_aligned_columns() {
        let p = plain();
        let block = format!(
            "{}\n{}\n{}\n{}",
            p.banner("not-goldfish · status", "— global"),
            p.section("memória"),
            p.kv("dados", "/tmp/x"),
            p.kvn("eventos capturados", "12 431"),
        );
        assert!(!block.contains('\x1b'), "block: {block:?}");
        let lines: Vec<&str> = block.lines().collect();
        let kv_line = lines[lines.len() - 2];
        let kvn_line = lines[lines.len() - 1];
        // Valor do kv começa na coluna 4 + LABEL_WIDTH; o kvn alinha o
        // número à direita dentro de NUM_WIDTH a partir da mesma coluna.
        assert_eq!(kv_line.find("/tmp/x"), Some(4 + LABEL_WIDTH));
        assert_eq!(kvn_line.chars().count(), 4 + LABEL_WIDTH + NUM_WIDTH);
    }

    #[test]
    fn kv_pads_label_to_the_shared_column() {
        let p = plain();
        assert_eq!(
            p.kv("dados", "/tmp/x"),
            "    dados                   /tmp/x"
        );
        assert_eq!(
            p.kvn("eventos capturados", "12 431"),
            "    eventos capturados        12 431"
        );
    }

    #[test]
    fn right_aligns_before_painting_so_columns_survive_ansi() {
        let p = ansi();
        let s = p.right(TEAL, "~92k", 8);
        assert_eq!(s, "\x1b[36m    ~92k\x1b[0m");
        assert_eq!(plain().right(TEAL, "~92k", 8), "    ~92k");
    }

    #[test]
    fn bar_fills_in_proportion_and_clamps() {
        let p = plain();
        assert_eq!(p.bar(0.0, 4), "░░░░");
        assert_eq!(p.bar(0.5, 4), "██░░");
        assert_eq!(p.bar(1.0, 4), "████");
        assert_eq!(p.bar(2.0, 4), "████");
        assert_eq!(p.bar(-1.0, 4), "░░░░");
        assert_eq!(p.bar(f64::NAN, 4), "░░░░");
        // Com cor: cheio violeta, vazio muted, mesmas colunas visíveis.
        assert_eq!(
            ansi().bar(0.5, 4),
            "\x1b[38;5;135m██\x1b[0m\x1b[90m░░\x1b[0m"
        );
    }

    #[test]
    fn resolve_honors_force_switches_before_everything() {
        assert!(resolve(Some("always"), None, true, Some("0"), false));
        assert!(!resolve(Some("never"), Some("1"), false, None, true));
        assert!(resolve(None, Some("1"), true, Some("0"), false));
        assert!(!resolve(None, Some("0"), false, None, false));
    }

    #[test]
    fn resolve_honors_no_color_and_clicolor_zero() {
        assert!(!resolve(None, None, true, None, true));
        assert!(!resolve(None, None, false, Some("0"), true));
        assert!(!resolve(None, None, false, None, false));
        assert!(resolve(None, None, false, None, true));
        assert!(resolve(None, None, false, Some("1"), true));
    }
}
