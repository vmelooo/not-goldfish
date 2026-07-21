//! `ng sync-context`: (re)gera `.ng/` — a projeção commitável da memória
//! deste projeto.
//!
//! O banco SQLite é a fonte de verdade (invariante do projeto); `.ng/` é
//! uma projeção derivada, idempotente e regenerável dele — o mesmo
//! relacionamento que `ng wisdom --here --md` tem com o grafo, agora
//! materializado em arquivos versionáveis:
//!
//! - `context.md`   — grafo de sabedoria (`Store::export_graph_md`);
//! - `decisions.md` — entidades `kind='decision'` + memórias manuais;
//! - `config.toml`  — único arquivo editado à mão, criado só com `--init`.
//!
//! Disciplina de escrita: conteúdo gerado em memória, escrito num tmp e
//! movido com rename atômico (mesma disciplina do rewrite de transcripts em
//! `ng-sessions`). Um arquivo pré-existente SEM o header "gerado por
//! not-goldfish" é do usuário → erro claro, arquivo intocado — nunca
//! destruímos conteúdo que não criamos.

use std::path::{Path, PathBuf};

use anyhow::Context;
use ng_core::{paths, timeutil, Entity, Memory, Store};

use crate::ui::Palette;

/// Marcador que identifica um arquivo como derivado nosso — a guarda de
/// sobrescrita procura por ele antes de qualquer rename.
const GENERATED_MARKER: &str = "gerado por not-goldfish";

/// Quantas entidades pedir ao snapshot antes de filtrar por decisão, e
/// quantas memórias listar antes de filtrar por `manual` — folga para o
/// filtro sem materializar o banco inteiro.
const SNAPSHOT_LIMIT: usize = 500;

/// Corte de exibição por memória manual em `decisions.md` — o conteúdo
/// completo continua no banco, o arquivo é uma projeção legível.
const MEMORY_PREVIEW_CHARS: usize = 300;

pub fn sync_context(init: bool, dir: Option<PathBuf>) -> anyhow::Result<()> {
    let dir = match dir {
        Some(d) if d.is_absolute() => d,
        Some(d) => std::env::current_dir()?.join(d),
        None => std::env::current_dir()?,
    };
    let db = paths::db_path();
    if !db.exists() {
        anyhow::bail!(
            "banco não existe ainda ({}). Rode `ng install` e use uma sessão para capturar memória.",
            db.display()
        );
    }
    let store = Store::open_readonly(&db)?;
    let now = timeutil::fmt_datetime(now_epoch());
    run(&store, &dir, init, &db.display().to_string(), &now)
}

/// Núcleo testável: recebe o store e o "agora" já resolvidos, escreve
/// `<dir>/.ng/`. Idempotente — o único não-determinismo é a linha do
/// timestamp no header (por isso ela tem uma linha própria: diffs limpos).
fn run(store: &Store, dir: &Path, init: bool, db_display: &str, now: &str) -> anyhow::Result<()> {
    let project = dir.to_string_lossy().into_owned();
    let ng_dir = dir.join(".ng");
    std::fs::create_dir_all(&ng_dir).with_context(|| format!("criando {}", ng_dir.display()))?;

    let header = render_header(now, db_display, &project);

    // context.md — reuso direto do caminho de `ng wisdom --here --md`,
    // zero lógica nova de conteúdo.
    let graph_md = store.export_graph_md(Some(&project))?;
    let context_body = if graph_md.trim() == "# Grafo de sabedoria" {
        "# Grafo de sabedoria\n\ngrafo ainda vazio — capture sessões para ele se popular.\n"
            .to_string()
    } else {
        graph_md
    };
    write_derived_atomic(
        &ng_dir.join("context.md"),
        &format!("{header}\n{context_body}"),
    )?;

    // decisions.md — decisões extraídas do grafo + memórias manuais.
    let (entities, _edges) = store.graph_snapshot(Some(&project), None, 0, SNAPSHOT_LIMIT)?;
    let memories = store.list_memories(Some(&project), false, SNAPSHOT_LIMIT)?;
    let decisions_body = render_decisions(&entities, &memories);
    write_derived_atomic(
        &ng_dir.join("decisions.md"),
        &format!("{header}\n{decisions_body}"),
    )?;

    let p = Palette::detect();
    println!(
        "{} {}",
        p.ok("gerado:"),
        ng_dir.join("context.md").display()
    );
    println!(
        "{} {}",
        p.ok("gerado:"),
        ng_dir.join("decisions.md").display()
    );

    if init {
        let config = ng_dir.join("config.toml");
        if config.exists() {
            // Único arquivo editado à mão do diretório — jamais sobrescrito.
            println!(
                "{} {} {}",
                p.warn("mantido:"),
                config.display(),
                p.muted("(já existia)")
            );
        } else {
            write_atomic(&config, CONFIG_TEMPLATE)?;
            println!("{} {}", p.ok("gerado:"), config.display());
        }
    }
    Ok(())
}

/// Header obrigatório e idêntico nos dois derivados. O timestamp fica numa
/// linha própria: rodar 2x sem mudança no banco difere só nessa linha.
fn render_header(now: &str, db_display: &str, project: &str) -> String {
    format!(
        "<!-- {GENERATED_MARKER} v{} — NÃO EDITE À MÃO.\n     gerado em: {now}\n     Regenere com: ng sync-context\n     Fonte: {db_display} (projeto {project}) -->\n",
        env!("CARGO_PKG_VERSION"),
    )
}

/// Corpo de `decisions.md`. Determinístico: decisões por peso desc com
/// empate por nome; memórias na ordem estável do banco (mais recente
/// primeiro, empate por id).
fn render_decisions(entities: &[Entity], memories: &[Memory]) -> String {
    let mut out = String::from("# Decisões do projeto\n\n## Decisões extraídas\n\n");

    let mut decisions: Vec<&Entity> = entities.iter().filter(|e| e.kind == "decision").collect();
    decisions.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    if decisions.is_empty() {
        out.push_str("— nenhuma decisão extraída ainda (frases como \"decidimos …\" e \"vamos usar …\" viram decisões).\n");
    } else {
        for decision in decisions {
            out.push_str(&format!(
                "- **{}** — peso {:.1} · atualizada {}\n",
                decision.name,
                decision.weight,
                timeutil::fmt_date(decision.updated_at)
            ));
        }
        // Proveniência por evento de origem ainda não é registrada pelo
        // grafo (tabela entity_sources: adiada, ver plano 003 B.3) — dizer
        // isso é mais honesto que citar uma sessão que não conhecemos.
        out.push_str("\n(origem por evento anterior ao registro de proveniência)\n");
    }

    out.push_str("\n## Memórias manuais\n\n");
    let manual: Vec<&Memory> = memories.iter().filter(|m| m.manual).collect();
    if manual.is_empty() {
        out.push_str("— nenhuma memória manual neste projeto (adicione pela UI ou MCP).\n");
    } else {
        for memory in manual {
            let one_line: String = memory
                .content
                .replace('\n', " ")
                .chars()
                .take(MEMORY_PREVIEW_CHARS)
                .collect();
            out.push_str(&format!(
                "- [{} · #{}] {}\n",
                timeutil::fmt_date(memory.created_at),
                memory.id,
                one_line
            ));
        }
    }
    out
}

/// Escrita atômica com a guarda de derivado: se o destino já existe e NÃO
/// contém o header gerado, é um arquivo do usuário → erro, arquivo intocado.
/// Se contém o header (mesmo editado depois), o contrato do header vale e o
/// arquivo é regenerado — sem backup: o histórico é o git do usuário e o
/// dado-fonte continua no banco.
fn write_derived_atomic(target: &Path, content: &str) -> anyhow::Result<()> {
    if target.exists() {
        let existing = std::fs::read_to_string(target)
            .with_context(|| format!("lendo {}", target.display()))?;
        if !existing.contains(GENERATED_MARKER) {
            anyhow::bail!(
                "{} já existe e não foi gerado pelo not-goldfish — recusando sobrescrever.\n\
                 Mova/renomeie o arquivo (o conteúdo é seu) e rode `ng sync-context` de novo.",
                target.display()
            );
        }
    }
    write_atomic(target, content)
}

/// tmp + rename no mesmo diretório (rename atômico exige mesmo filesystem).
/// Mesma disciplina do rewrite de transcripts em `ng-sessions`: sufixo de
/// tmp único por chamada (duas execuções concorrentes não se atropelam),
/// fsync do conteúdo antes do rename e fsync do diretório depois (best
/// effort) — `.ng/` é regenerável, mas nunca deve ficar truncado.
fn write_atomic(target: &Path, content: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let file_name = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("caminho sem nome de arquivo: {}", target.display()))?
        .to_string_lossy();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = target.with_file_name(format!(
        ".{file_name}.tmp-{nanos}-{}-{seq}",
        std::process::id()
    ));
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("criando tmp {}", tmp.display()))?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("escrevendo {}", tmp.display()))?;
    std::fs::rename(&tmp, target)
        .with_context(|| format!("renomeando {} → {}", tmp.display(), target.display()))?;
    // Best effort: fsync do diretório para o rename sobreviver a um crash.
    if let Some(dir) = target.parent() {
        if let Ok(dir_handle) = std::fs::File::open(dir) {
            let _ = dir_handle.sync_all();
        }
    }
    Ok(())
}

/// Template do único arquivo editável à mão. As chaves espelham as env vars
/// do README; a leitura pelo hook ainda não existe (adiada até haver medição
/// de timing no hot path — plano 003 B.3/pergunta 2), e o template diz isso
/// em vez de fingir efeito.
const CONFIG_TEMPLATE: &str = r#"# not-goldfish — config por projeto (este arquivo É editado à mão)
#
# ATENÇÃO: ainda não lido pelo hook — por enquanto use as env vars
# equivalentes (a leitura por projeto entra quando o custo no hot path
# <5ms do ng-hook for medido; ver plano 003).

[inject]
# limit = 3          # NG_INJECT_LIMIT
# budget = 600       # NG_INJECT_BUDGET

[dispatch]
# overrides herdam o formato de ~/.not-goldfish/dispatch.toml
"#;

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ng_core::Event;

    fn temp_store(dir: &Path) -> Store {
        let store = Store::open(&dir.join("ng.db")).unwrap();
        store
            .add_manual_memory(
                "/tmp/proj-x",
                "sempre rodar clippy antes do commit",
                "clippy",
            )
            .unwrap();
        // Um prompt com marcador de decisão + ingestão no grafo, para a
        // seção de decisões extraídas ter conteúdo real.
        let event = Event {
            session_id: "s1".to_string(),
            project: "/tmp/proj-x".to_string(),
            harness: "claude-code".to_string(),
            kind: "prompt".to_string(),
            content: "decidimos usar sqlite com wal como fonte de verdade".to_string(),
            tags: "sqlite wal".to_string(),
            meta: None,
            created_at: 1_784_368_800,
        };
        store.insert_event(&event).unwrap();
        store.ingest_graph(&event).unwrap();
        store
    }

    fn project_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn generates_the_three_contract_files_with_the_generated_header() {
        let db_dir = project_dir();
        let store = temp_store(db_dir.path());
        let proj = project_dir();

        run(&store, proj.path(), true, "test-db", "2026-07-21 12:00Z").unwrap();

        for name in ["context.md", "decisions.md"] {
            let content = std::fs::read_to_string(proj.path().join(".ng").join(name)).unwrap();
            assert!(
                content.starts_with(&format!("<!-- {GENERATED_MARKER}")),
                "{name} deve começar com o header de derivado"
            );
            assert!(content.contains("gerado em: 2026-07-21 12:00Z"));
        }
        let config = std::fs::read_to_string(proj.path().join(".ng/config.toml")).unwrap();
        assert!(config.contains("NG_INJECT_LIMIT"));
    }

    #[test]
    fn is_idempotent_for_a_fixed_timestamp_and_only_the_timestamp_varies() {
        let db_dir = project_dir();
        let store = temp_store(db_dir.path());
        let proj = project_dir();
        let context = proj.path().join(".ng/context.md");

        run(&store, proj.path(), false, "test-db", "2026-07-21 12:00Z").unwrap();
        let first = std::fs::read_to_string(&context).unwrap();
        run(&store, proj.path(), false, "test-db", "2026-07-21 12:00Z").unwrap();
        let second = std::fs::read_to_string(&context).unwrap();
        assert_eq!(first, second, "mesmo banco + mesmo agora ⇒ mesmos bytes");

        run(&store, proj.path(), false, "test-db", "2026-07-21 13:30Z").unwrap();
        let third = std::fs::read_to_string(&context).unwrap();
        let diff: Vec<(&str, &str)> = first
            .lines()
            .zip(third.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(diff.len(), 1, "só a linha do timestamp pode diferir");
        assert!(diff[0].0.contains("gerado em:"));
    }

    #[test]
    fn refuses_to_clobber_a_file_without_the_generated_header() {
        let db_dir = project_dir();
        let store = temp_store(db_dir.path());
        let proj = project_dir();
        let ng_dir = proj.path().join(".ng");
        std::fs::create_dir_all(&ng_dir).unwrap();
        let user_file = ng_dir.join("context.md");
        std::fs::write(&user_file, "# minhas notas preciosas\n").unwrap();

        let err = run(&store, proj.path(), false, "test-db", "2026-07-21 12:00Z")
            .expect_err("arquivo do usuário sem header deve abortar");
        assert!(err.to_string().contains("recusando sobrescrever"));
        assert_eq!(
            std::fs::read_to_string(&user_file).unwrap(),
            "# minhas notas preciosas\n",
            "o arquivo do usuário deve ficar intocado"
        );
    }

    #[test]
    fn a_previously_generated_file_is_regenerated_even_if_hand_edited() {
        let db_dir = project_dir();
        let store = temp_store(db_dir.path());
        let proj = project_dir();

        run(&store, proj.path(), false, "test-db", "2026-07-21 12:00Z").unwrap();
        let context = proj.path().join(".ng/context.md");
        let mut edited = std::fs::read_to_string(&context).unwrap();
        edited.push_str("\nedição à mão que o header proíbe\n");
        std::fs::write(&context, &edited).unwrap();

        run(&store, proj.path(), false, "test-db", "2026-07-21 12:00Z").unwrap();
        let regenerated = std::fs::read_to_string(&context).unwrap();
        assert!(!regenerated.contains("edição à mão"));
    }

    #[test]
    fn init_never_overwrites_an_existing_config_toml() {
        let db_dir = project_dir();
        let store = temp_store(db_dir.path());
        let proj = project_dir();
        let ng_dir = proj.path().join(".ng");
        std::fs::create_dir_all(&ng_dir).unwrap();
        std::fs::write(ng_dir.join("config.toml"), "[inject]\nlimit = 7\n").unwrap();

        run(&store, proj.path(), true, "test-db", "2026-07-21 12:00Z").unwrap();
        assert_eq!(
            std::fs::read_to_string(ng_dir.join("config.toml")).unwrap(),
            "[inject]\nlimit = 7\n"
        );
    }

    #[test]
    fn decisions_md_carries_extracted_decisions_and_manual_memories() {
        let db_dir = project_dir();
        let store = temp_store(db_dir.path());
        let proj = project_dir();

        // O escopo do banco é o project do dir; nossos dados de teste moram
        // em /tmp/proj-x, então gere apontando um dir "fake" com o mesmo
        // caminho não dá — em vez disso, renderize direto do snapshot.
        let (entities, _) = store
            .graph_snapshot(Some("/tmp/proj-x"), None, 0, SNAPSHOT_LIMIT)
            .unwrap();
        let memories = store
            .list_memories(Some("/tmp/proj-x"), false, SNAPSHOT_LIMIT)
            .unwrap();
        let body = render_decisions(&entities, &memories);
        assert!(
            body.contains("decidimos usar sqlite"),
            "decisão extraída deveria aparecer: {body}"
        );
        assert!(body.contains("sempre rodar clippy antes do commit"));
        assert!(body.contains("anterior ao registro de proveniência"));
        drop(proj);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_behind() {
        let proj = project_dir();
        let target = proj.path().join("out.md");
        write_atomic(&target, "conteúdo").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "conteúdo");
        assert!(!proj.path().join(".out.md.tmp").exists());
    }
}
