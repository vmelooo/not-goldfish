//! Runs the with/without study, prints a table, and writes machine-readable
//! results to `crates/ng-bench/results/latest.json`.

use std::path::PathBuf;

use ng_bench::harness::{ArmSummary, StudyResults};
use ng_bench::run_full_study;

fn arm_row(a: &ArmSummary) -> String {
    format!(
        "{:<32} {:>6.0}% {:>9.2} {:>6.2} {:>8.2} {:>12.0} {:>13.1}% {:>11.1}%",
        a.name,
        a.accuracy * 100.0,
        a.recall_at_k,
        a.mrr,
        a.precision_at_k,
        a.avg_injected_tokens,
        a.token_savings_pct,
        a.token_savings_vs_oracle_pct,
    )
}

fn print_table(r: &StudyResults) {
    println!("\n=== not-goldfish: estudo COM vs SEM a ferramenta (LoCoMo-style) ===");
    println!(
        "tarefas={}  eventos={}  top_k={}  embedder_hash={}  embedder_m2v={}",
        r.task_count,
        r.corpus_events,
        r.top_k,
        r.embedder_hash_id,
        r.embedder_m2v_id.as_deref().unwrap_or("(desligado)"),
    );
    println!(
        "\n{:<32} {:>7} {:>9} {:>6} {:>8} {:>12} {:>14} {:>12}",
        "arm", "acc", "recall@k", "mrr", "prec@k", "inj_tokens", "econ_vs_full", "econ_vs_orac",
    );
    println!("{}", "-".repeat(108));
    println!("{}", arm_row(&r.without_no_memory));
    println!("{}", arm_row(&r.without_full_context));
    println!("{}", arm_row(&r.with_fts));
    println!("{}", arm_row(&r.with_hybrid_hash));
    if let Some(m2v) = &r.with_hybrid_m2v {
        println!("{}", arm_row(m2v));
    } else {
        println!(
            "{:<32} {:>7}",
            "WITH hybrid (model2vec)", "(pulado: feature/modelo ausente)"
        );
    }

    println!(
        "\ntokens: full-context (SEM, todo o histórico)={:.0}  |  oracle (sessão certa conhecida)={:.0}",
        r.without_full_context.avg_full_context_tokens, r.without_full_context.avg_oracle_tokens,
    );

    // The fairness-critical view: same arms, split by task class. Averaging
    // lexical-overlap and semantic-gap together hides where FTS is blind.
    println!("\n=== SPLIT por classe de tarefa (o que a média esconde) ===");
    for c in &r.by_class {
        println!("\n-- classe: {} ({} tarefas) --", c.class, c.task_count);
        println!(
            "{:<32} {:>7} {:>9} {:>6} {:>8} {:>12} {:>14}",
            "arm", "acc", "recall@k", "mrr", "prec@k", "inj_tokens", "econ_found",
        );
        println!("{}", "-".repeat(92));
        let row = |a: &ArmSummary| {
            println!(
                "{:<32} {:>6.0}% {:>9.2} {:>6.2} {:>8.2} {:>12.0} {:>13.1}%",
                a.name,
                a.accuracy * 100.0,
                a.recall_at_k,
                a.mrr,
                a.precision_at_k,
                a.avg_injected_tokens,
                a.token_savings_pct_on_found,
            );
        };
        row(&c.without_full_context);
        row(&c.with_fts);
        row(&c.with_hybrid_hash);
        match &c.with_hybrid_m2v {
            Some(m2v) => row(m2v),
            None => println!(
                "{:<32} {:>7}",
                "WITH hybrid (model2vec)", "(N/A: sem NG_EMBED_MODEL)"
            ),
        }
    }
    println!(
        "\nnota: `econ_found` = economia de tokens só nas tarefas ONDE o ouro foi \
         entregue.\n      Num miss a injeção é ~0 token — sem isto, uma FALHA \
         apareceria como ~100% de economia."
    );

    println!("\n-- grounding (resposta apoiada por proveniência recuperada) --");
    println!(
        "  WITH fts          : {:>5.0}%",
        r.with_fts.grounded_rate * 100.0
    );
    println!(
        "  WITH hybrid(hash) : {:>5.0}%",
        r.with_hybrid_hash.grounded_rate * 100.0
    );

    println!("\n-- referência mem0 (LoCoMo) --");
    println!(
        "  economia de tokens vs full-context: ~{:.0}%  |  acurácia mem0 {:.1}% vs full-context {:.1}%",
        r.mem0_reference.token_savings_pct_vs_full_context,
        r.mem0_reference.accuracy_mem0,
        r.mem0_reference.accuracy_full_context,
    );

    println!("\n-- por tarefa (onde ganhamos/perdemos) --");
    println!(
        "{:<26} {:<16} {:>8} {:>6} {:>8} {:>7}",
        "tarefa", "classe", "full_ctx", "fts", "hy.hash", "hy.m2v"
    );
    for row in &r.per_task {
        let m2v = match row.hybrid_m2v_found {
            Some(true) => "  ok",
            Some(false) => "MISS",
            None => "   -",
        };
        println!(
            "{:<26} {:<16} {:>8} {:>6} {:>8} {:>7}",
            row.task,
            row.class,
            row.full_context_tokens,
            if row.fts_found { "ok" } else { "MISS" },
            if row.hybrid_hash_found { "ok" } else { "MISS" },
            m2v,
        );
    }
}

fn main() -> anyhow::Result<()> {
    let db = std::env::temp_dir().join(format!("ng-bench-{}.db", std::process::id()));
    let results = run_full_study(&db)?;

    print_table(&results);

    let out_path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("results/latest.json");
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&results)?;
    std::fs::write(&out_path, json)?;
    println!("\nresultados JSON escritos em: {}", out_path.display());

    Ok(())
}
