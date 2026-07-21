//! `ng saver`: gerenciamento dos savers externos (plano 004) — `init`,
//! `list` e o gate de medição `bench`.
//!
//! O modelo de honestidade do bench (plano 004 §4): o baseline NÃO é o
//! conteúdo integral, é o nosso stub nativo `[ng-evicted: …]` — a higiene
//! lossless já existe e já economiza; o saver só recebe crédito pelo
//! *delta* sobre ela, descontada a inflação de retrieval. Tokens sempre
//! pela nossa heurística ×4 bytes; auto-relatos do saver nunca entram.

use std::time::Instant;

use anyhow::Context;
use clap::Subcommand;
use ng_adapters::saver_cli::build_one;
use ng_adapters::savers::{SaversConfig, Transport, SAVERS_TOML_TEMPLATE};
use ng_core::saver::estimate_tokens;
use ng_core::{paths, Store};

use crate::ui::Palette;

#[derive(Debug, Subcommand)]
pub enum SaverCommand {
    /// Escreve o ~/.not-goldfish/savers.toml comentado (nunca sobrescreve)
    Init,
    /// Lista os savers configurados e o estado do gate de medição
    List {
        /// Saída JSON estável (para scripts)
        #[arg(long)]
        json: bool,
    },
    /// Mede um saver contra tool_outputs reais do banco e promove a
    /// "trusted" só se for líquido-positivo sobre o stub nativo
    Bench {
        /// Nome do saver (como definido no savers.toml global)
        name: String,
        /// Máximo de eventos reais na amostra
        #[arg(long, default_value_t = 50)]
        sample: usize,
    },
}

pub fn saver(action: SaverCommand) -> anyhow::Result<()> {
    match action {
        SaverCommand::Init => init(),
        SaverCommand::List { json } => list(json),
        SaverCommand::Bench { name, sample } => bench(&name, sample),
    }
}

fn config_path() -> std::path::PathBuf {
    paths::data_dir().join("savers.toml")
}

/// Config global; arquivo ausente = nenhum saver (não é erro). Comandos só
/// nascem aqui — o `.ng/config.toml` de projeto pode no máximo ligar/
/// desligar por nome (`SaversConfig::apply_project_toggles`), regra
/// aplicada por quem carrega config de projeto, nunca relaxada aqui.
fn load_config() -> anyhow::Result<SaversConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(SaversConfig::default());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("lendo {}", path.display()))?;
    SaversConfig::from_global_toml(&raw).map_err(|e| anyhow::anyhow!("{e}"))
}

fn init() -> anyhow::Result<()> {
    let path = config_path();
    let p = Palette::detect();
    if path.exists() {
        println!("já existe: {} (não sobrescrevo)", p.bold(path.display()));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, SAVERS_TOML_TEMPLATE)?;
    println!("{} {}", p.ok("escrito:"), p.bold(path.display()));
    println!(
        "{}",
        p.dim("edite, ligue um saver (enabled = true) e rode `ng saver bench <nome>`.")
    );
    Ok(())
}

fn list(json: bool) -> anyhow::Result<()> {
    let config = load_config()?;
    // Estado do gate vem do banco; sem banco = nunca medido.
    let states: Vec<(String, String, i64)> = {
        let db = paths::db_path();
        if db.exists() {
            Store::open_readonly(&db)?.saver_states()?
        } else {
            Vec::new()
        }
    };
    let status_of = |name: &str| -> String {
        states
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, s, _)| s.clone())
            .unwrap_or_else(|| "nunca medido".to_string())
    };

    if json {
        // Contrato de scripts: nomes de campo estáveis, mudanças aditivas.
        let out = serde_json::json!({
            "config": config_path().display().to_string(),
            "savers": config.savers.iter().map(|s| serde_json::json!({
                "name": s.name,
                "enabled": s.enabled,
                "transport": match s.transport { Transport::Cli => "cli", Transport::Mcp => "mcp" },
                "status": status_of(&s.name),
                "timeout_ms": s.timeout_ms,
                "budget_tokens": s.budget_tokens,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let p = Palette::detect();
    if config.savers.is_empty() {
        println!("{}", p.muted("nenhum saver configurado."));
        println!(
            "{}",
            p.dim(format!(
                "rode `ng saver init` para gerar {} comentado.",
                config_path().display()
            ))
        );
        return Ok(());
    }
    println!("savers em {}:", p.dim(config_path().display()));
    for s in &config.savers {
        let transport = match s.transport {
            Transport::Cli => "cli",
            Transport::Mcp => "mcp",
        };
        let toggle = if s.enabled {
            p.ok("on")
        } else {
            p.muted("off")
        };
        let gate = match status_of(&s.name).as_str() {
            "trusted" => p.ok("trusted"),
            "measured" => p.warn("measured"),
            other => p.muted(other),
        };
        println!(
            "  {} · {} · {} · gate: {} · timeout {}ms · budget ~{} tok",
            p.bold(&s.name),
            toggle,
            transport,
            gate,
            s.timeout_ms,
            s.budget_tokens,
        );
    }
    println!(
        "{}",
        p.dim("(um saver só roda com enabled = true E gate \"trusted\" — `ng saver bench <nome>`)")
    );
    Ok(())
}

/// Frações de retrieval expostas lado a lado no veredito — esconder a
/// sensibilidade a `r` é o vício que o plano existe para evitar. A do meio
/// é a que decide.
const RETRIEVAL_FRACTIONS: [f64; 3] = [0.05, 0.15, 0.30];
const DECISIVE_R: f64 = 0.15;
/// Mesmo piso do worker do `ngd` (`SAVER_MIN_CONTENT_BYTES`): o bench mede
/// o workload que o saver de fato veria.
const BENCH_MIN_CONTENT_BYTES: usize = 2048;

fn bench(name: &str, sample: usize) -> anyhow::Result<()> {
    let config = load_config()?;
    let spec = config
        .savers
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("saver {name} não definido em {}", config_path().display()))?;
    let db = paths::db_path();
    if !db.exists() {
        anyhow::bail!("banco não existe ainda — use sessões para capturar tool_outputs antes");
    }
    // RW: o veredito grava o status do gate (measured/trusted) no fim.
    let store = Store::open(&db)?;
    let items = store.sample_tool_outputs(BENCH_MIN_CONTENT_BYTES, sample)?;
    if items.is_empty() {
        anyhow::bail!(
            "nenhum tool_output ≥ {BENCH_MIN_CONTENT_BYTES} bytes no banco para amostrar"
        );
    }
    let budget = spec.budget_tokens;
    let timeout_ms = spec.timeout_ms;
    // O transporte certo (CLI ou MCP) sai do spec — o bench é agnóstico.
    let saver = build_one(spec.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut latencies_ms: Vec<u128> = Vec::new();
    let mut failures = 0usize;
    let mut roundtrip_fail = 0usize;
    let mut tokens_before_sum = 0i64;
    let mut digest_sum = 0i64;
    let mut stub_sum = 0i64;
    let mut ok = 0usize;

    for content in &items {
        let t0 = Instant::now();
        match saver.compress(content, budget) {
            Ok(c) => {
                latencies_ms.push(t0.elapsed().as_millis());
                ok += 1;
                tokens_before_sum += c.tokens_before;
                digest_sum += c.tokens_after;
                stub_sum += native_stub_tokens(content);
                if let Some(r) = &c.reversible_ref {
                    // Round-trip verificado de verdade, não confiado na
                    // promessa: retrieve(ref) tem que devolver o original
                    // byte-a-byte.
                    match saver.retrieve(r) {
                        Ok(back) if back == *content => {}
                        _ => roundtrip_fail += 1,
                    }
                }
            }
            Err(_) => {
                latencies_ms.push(t0.elapsed().as_millis());
                failures += 1;
            }
        }
    }

    latencies_ms.sort_unstable();
    let pct = |p: f64| -> u128 {
        if latencies_ms.is_empty() {
            0
        } else {
            let idx = ((latencies_ms.len() as f64 * p).ceil() as usize)
                .saturating_sub(1)
                .min(latencies_ms.len() - 1);
            latencies_ms[idx]
        }
    };
    let (p50, p95) = (pct(0.50), pct(0.95));
    let delta = stub_sum - digest_sum;
    let net = |r: f64| delta as f64 - r * tokens_before_sum as f64;

    let p = Palette::detect();
    println!(
        "{} · {} itens reais · budget ~{budget} tok",
        p.gold(format!("bench de {name}")),
        items.len()
    );
    let failures_c = if failures > 0 {
        p.err(failures)
    } else {
        p.muted(failures)
    };
    let roundtrip_c = if roundtrip_fail > 0 {
        p.err(roundtrip_fail)
    } else {
        p.muted(roundtrip_fail)
    };
    println!(
        "  falhas: {failures_c}/{} · round-trip quebrado: {roundtrip_c}",
        items.len()
    );
    println!("  latência: p50 {p50}ms · p95 {p95}ms (timeout {timeout_ms}ms)");
    println!(
        "{}",
        p.dim("  tokens (heurística ×4 bytes nossa — nunca auto-relato do saver):")
    );
    println!(
        "    originais Σ {} · stub nativo Σ {} · digest Σ {}",
        p.teal(tokens_before_sum),
        p.teal(stub_sum),
        p.teal(digest_sum)
    );
    println!(
        "  delta vs o que JÁ fazemos (stub − digest): {}",
        p.teal(delta)
    );
    for r in RETRIEVAL_FRACTIONS {
        let net_r = net(r);
        let net_c = if net_r > 0.0 {
            p.ok(format!("{net_r:+.0}"))
        } else {
            p.err(format!("{net_r:+.0}"))
        };
        println!("    ganho líquido com r = {r:.2} (fração re-lida): {net_c} tok");
    }

    let passed = ok > 0
        && failures == 0
        && roundtrip_fail == 0
        && (p95 as u64) < timeout_ms
        && net(DECISIVE_R) > 0.0;
    let status = if passed { "trusted" } else { "measured" };
    store.set_saver_status(name, status)?;
    if passed {
        println!(
            "{}",
            p.ok(format!(
                "veredito: PASSOU — {name} promovido a \"trusted\" (o worker do ngd passa a computar digests)."
            ))
        );
    } else {
        println!(
            "{}",
            p.warn(format!(
                "veredito: NÃO passou — {name} fica \"measured\" (nenhum digest entra em stub)."
            ))
        );
        println!(
            "{}",
            p.dim(format!(
                "  critérios: 0 falhas, round-trip 100%, p95 < timeout, ganho líquido (r = {DECISIVE_R}) > 0."
            ))
        );
    }
    println!(
        "{}",
        p.dim(format!(
            "caveat: estimativas com a nossa heurística ×4 bytes sobre {} itens; o rebaixamento \
             automático (\"demoted\") pela janela real do gain_ledger ainda não está implementado \
             nesta fase — re-rode o bench se o workload mudar.",
            items.len()
        ))
    );
    Ok(())
}

/// Estimativa de tokens do stub nativo que a higiene produziria para este
/// conteúdo — o baseline honesto do bench. Réplica de forma (não de código)
/// de `ng_sessions::hygiene::stub_for` + `derive_search_hint`: formato
/// `[ng-evicted: <kind> ~<N>tok — recupere com: ng search <5 palavras> |
/// id interno preservado no banco]`.
fn native_stub_tokens(content: &str) -> i64 {
    let hint: Vec<&str> = content
        .split_whitespace()
        .filter(|w| w.chars().filter(|c| c.is_alphanumeric()).count() >= 3)
        .take(5)
        .collect();
    let stub = format!(
        "[ng-evicted: tool_output ~{}tok — recupere com: ng search {} | id interno preservado no banco]",
        estimate_tokens(content),
        if hint.is_empty() { "item".to_string() } else { hint.join(" ") },
    );
    estimate_tokens(&stub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_stub_tokens_is_small_and_positive() {
        let content = "linha de saida de ferramenta ".repeat(100);
        let t = native_stub_tokens(&content);
        assert!(t > 0);
        assert!(
            t < estimate_tokens(&content),
            "o stub nativo é sempre menor que o conteúdo que substitui"
        );
        // Ordem de grandeza: um stub é ~1 linha, não centenas de tokens.
        assert!(t < 60, "stub nativo estimado em {t} tokens");
    }
}
