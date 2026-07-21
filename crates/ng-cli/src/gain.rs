//! `ng gain`: benefício acumulado honesto desde a adoção.
//!
//! Modelo de honestidade (plano 003, informado por `docs/research/
//! tooling-gains.md`): a única linha chamada de "economia" é a higiene de
//! contexto — tokens que estavam no transcript vivo e foram trocados por
//! stubs, líquidos do custo dos próprios stubs, contados UMA vez (piso
//! conservador). Injeção proativa é reportada como **custo declarado**
//! (tokens que entraram no prompt), nunca somada como economia — o
//! benefício dela é real mas não mensurável sem A/B pareado. Não existe
//! linha de cache/dedup porque o mecanismo não existe.

use ng_core::{paths, timeutil, Store};

use crate::i18n::{fill, Msgs};
use crate::ui::{Palette, GOLD};

/// Chave estável no JSON dizendo qual contrafactual está sendo afirmado.
const GAIN_MODEL: &str = "net-stub-tokens-counted-once";

/// Agregado pronto para exibir, derivado de `stats_scoped` + `gain_summary`.
#[derive(Debug, Default, PartialEq)]
struct GainReport {
    events: i64,
    sessions: i64,
    stored_tokens_est: i64,
    using_since_epoch: Option<i64>,
    inject_prompts: i64,
    inject_items: i64,
    inject_tokens_est: i64,
    precompact_runs: i64,
    clear_runs: i64,
    hygiene_items: i64,
    tokens_saved_est: i64,
}

impl GainReport {
    /// A fórmula do ganho: `tokens_saved_est = Σ tokens (kind ∈ {evict,
    /// clear})` — e nada mais. `inject` vira as linhas de custo declarado.
    /// Kinds desconhecidos (versões futuras) são ignorados, nunca somados.
    fn from_rows(
        stats: (i64, i64, i64, Option<i64>),
        rows: &[(String, i64, i64, i64)],
    ) -> GainReport {
        let (events, sessions, stored_tokens_est, using_since_epoch) = stats;
        let mut report = GainReport {
            events,
            sessions,
            stored_tokens_est,
            using_since_epoch,
            ..GainReport::default()
        };
        for (kind, runs, items, tokens) in rows {
            match kind.as_str() {
                "inject" => {
                    report.inject_prompts += runs;
                    report.inject_items += items;
                    report.inject_tokens_est += tokens;
                }
                "evict" => {
                    report.precompact_runs += runs;
                    report.hygiene_items += items;
                    report.tokens_saved_est += tokens;
                }
                "clear" => {
                    report.clear_runs += runs;
                    report.hygiene_items += items;
                    report.tokens_saved_est += tokens;
                }
                _ => {}
            }
        }
        report
    }

    fn has_inject_data(&self) -> bool {
        self.inject_prompts > 0
    }

    fn has_hygiene_data(&self) -> bool {
        self.precompact_runs > 0 || self.clear_runs > 0
    }
}

pub fn gain(here: bool, json: bool, since: Option<String>) -> anyhow::Result<()> {
    let m = Msgs::get();
    let db = paths::db_path();
    if !db.exists() {
        anyhow::bail!("{}", fill(m.db_missing, &[("{path}", &db.display())]));
    }
    let since_epoch = match &since {
        Some(s) => Some(timeutil::parse_date(s).ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                fill(m.gain_since_invalid, &[("{s}", &format!("{s:?}"))])
            )
        })?),
        None => None,
    };
    let store = Store::open_readonly(&db)?;
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let project = here.then_some(cwd.as_str());

    let stats = store.stats_scoped(project, since_epoch)?;
    let rows = store.gain_summary(project, since_epoch)?;
    let report = GainReport::from_rows(stats, &rows);

    if json {
        print_json(&report, project, since_epoch);
    } else {
        print_text(&report, project, since_epoch, &db.display().to_string());
    }
    Ok(())
}

fn print_json(report: &GainReport, project: Option<&str>, since: Option<i64>) {
    let out = serde_json::json!({
        "scope": project.unwrap_or("global"),
        "since_epoch": since,
        "using_since_epoch": report.using_since_epoch,
        "events": report.events,
        "sessions": report.sessions,
        "stored_tokens_est": report.stored_tokens_est,
        "inject": {
            "prompts": report.inject_prompts,
            "items": report.inject_items,
            "tokens_est": report.inject_tokens_est,
        },
        "hygiene": {
            "precompact_runs": report.precompact_runs,
            "clear_runs": report.clear_runs,
            "items": report.hygiene_items,
            "tokens_saved_est": report.tokens_saved_est,
        },
        "model": GAIN_MODEL,
    });
    println!("{out}");
}

fn print_text(report: &GainReport, project: Option<&str>, since: Option<i64>, db_display: &str) {
    let m = Msgs::get();
    let p = Palette::detect();
    let scope = match project {
        Some(proj) => fill(m.gain_scope_project, &[("{proj}", &proj)]),
        None => m.gain_scope_global.to_string(),
    };
    println!("{}", p.banner(m.gain_banner, &scope));
    if let Some(since) = since {
        println!(
            "{}",
            p.dim(fill(
                m.gain_counting_from,
                &[("{date}", &timeutil::fmt_date(since))]
            ))
        );
    }
    println!();
    match report.using_since_epoch {
        Some(first) => println!(
            "{}",
            p.kv(
                m.gain_using_since,
                p.bold(fill(
                    m.gain_using_since_value,
                    &[
                        ("{date}", &timeutil::fmt_date(first)),
                        ("{days}", &days_since(first, now_epoch())),
                    ]
                ))
            )
        ),
        None => println!(
            "{}",
            p.kv(m.gain_using_since, p.muted(m.gain_using_since_none))
        ),
    }
    println!();

    println!("{}", p.section(m.gain_sec_memory));
    println!(
        "{}",
        p.kvn(m.gain_events_captured, fmt_count(report.events))
    );
    println!(
        "{}",
        p.kvn(m.gain_sessions_tracked, fmt_count(report.sessions))
    );
    println!(
        "{}",
        p.kvn(m.gain_tokens_stored, fmt_tokens(report.stored_tokens_est))
    );
    println!();

    println!("{}", p.section(m.gain_sec_inject));
    if report.has_inject_data() {
        println!(
            "{}",
            p.kvn(m.gain_prompts_served, fmt_count(report.inject_prompts))
        );
        println!(
            "{}",
            p.kvn(m.gain_memories_served, fmt_count(report.inject_items))
        );
        println!(
            "{}",
            p.kvn(m.gain_tokens_injected, fmt_tokens(report.inject_tokens_est))
        );
    } else {
        println!("{}", p.muted(m.gain_no_data));
    }
    println!();

    println!("{}", p.section(m.gain_sec_hygiene));
    if report.has_hygiene_data() {
        println!(
            "{}",
            p.kvn(m.gain_passes_precompact, fmt_count(report.precompact_runs))
        );
        println!(
            "{}",
            p.kvn(m.gain_passes_clear, fmt_count(report.clear_runs))
        );
        println!(
            "{}",
            p.kvn(m.gain_items_stubbed, fmt_count(report.hygiene_items))
        );
        // A única linha chamada de economia — por isso a única em dourado.
        println!(
            "{}",
            p.kv(
                m.gain_tokens_saved,
                p.right(GOLD, fmt_tokens(report.tokens_saved_est), 8)
            )
        );
        // Proporção líquida: quanto da economia resta depois de descontar o
        // custo declarado da injeção. Só existe quando os dois lados têm
        // dado — senão vira fração contra um denominador imaginário.
        if report.has_inject_data() {
            let net = report.tokens_saved_est - report.inject_tokens_est;
            let frac = if report.tokens_saved_est > 0 {
                net as f64 / report.tokens_saved_est as f64
            } else {
                0.0
            };
            let signed = format!(
                "{}{}",
                if net >= 0 { "+" } else { "-" },
                fmt_tokens(net.abs())
            );
            println!(
                "{}",
                p.kv(
                    m.gain_net_ratio,
                    format!("{} {:.0}% ({signed})", p.bar(frac, 16), frac * 100.0)
                )
            );
        }
    } else {
        println!("{}", p.muted(m.gain_no_data));
    }
    println!();

    // Rodapé fixo — a honestidade é o produto (plano 003 A.3).
    println!("{}", p.muted(m.gain_footer_estimates));
    println!(
        "{}",
        p.muted(fill(m.gain_footer_source, &[("{db}", &db_display)]))
    );
}

/// Dias inteiros (piso) entre dois epochs — "usando desde X (N dias)".
fn days_since(first: i64, now: i64) -> i64 {
    ((now - first).max(0)) / 86_400
}

/// Contagem exata com separador de milhar por espaço fino de ASCII
/// (`12 431`), como no `ng gain` planejado.
fn fmt_count(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(' ');
        }
        grouped.push(c);
    }
    if n < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Tokens são estimativa (~4 bytes/token), então sempre aproximados:
/// `~842`, `~92k`, `~1.9M`.
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("~{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("~{}k", (n as f64 / 1_000.0).round() as i64)
    } else {
        format!("~{n}")
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_only_counts_hygiene_as_savings_and_inject_as_cost() {
        let rows = vec![
            ("clear".to_string(), 6, 90, 100_000),
            ("evict".to_string(), 31, 124, 218_000),
            ("inject".to_string(), 412, 1_108, 92_000),
        ];
        let report = GainReport::from_rows((12_431, 87, 1_900_000, Some(1_000)), &rows);

        // economia = Σ tokens de evict + clear, e NADA de inject.
        assert_eq!(report.tokens_saved_est, 318_000);
        assert_eq!(report.precompact_runs, 31);
        assert_eq!(report.clear_runs, 6);
        assert_eq!(report.hygiene_items, 214);
        // injeção fica inteira do lado do custo.
        assert_eq!(report.inject_prompts, 412);
        assert_eq!(report.inject_items, 1_108);
        assert_eq!(report.inject_tokens_est, 92_000);
        assert_eq!(report.using_since_epoch, Some(1_000));
    }

    #[test]
    fn formula_ignores_unknown_kinds_instead_of_summing_them() {
        // Um kind futuro (p.ex. "cache") nunca pode inflar a economia
        // retroativamente sem uma decisão explícita nesta função.
        let rows = vec![("cache".to_string(), 10, 10, 999_999)];
        let report = GainReport::from_rows((0, 0, 0, None), &rows);
        assert_eq!(report.tokens_saved_est, 0);
        assert_eq!(report.inject_tokens_est, 0);
    }

    #[test]
    fn empty_ledger_reports_no_data_not_zero_measurements() {
        let report = GainReport::from_rows((5, 1, 400, Some(1_000)), &[]);
        assert!(!report.has_inject_data());
        assert!(!report.has_hygiene_data());
    }

    #[test]
    fn fmt_count_groups_thousands_with_spaces() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(987), "987");
        assert_eq!(fmt_count(12_431), "12 431");
        assert_eq!(fmt_count(1_234_567), "1 234 567");
    }

    #[test]
    fn fmt_tokens_is_always_marked_approximate() {
        assert_eq!(fmt_tokens(842), "~842");
        assert_eq!(fmt_tokens(92_400), "~92k");
        assert_eq!(fmt_tokens(318_000), "~318k");
        assert_eq!(fmt_tokens(1_900_000), "~1.9M");
    }

    #[test]
    fn days_since_floors_and_never_goes_negative() {
        assert_eq!(days_since(0, 86_400 * 80 + 3_600), 80);
        assert_eq!(days_since(10_000, 0), 0);
    }
}
