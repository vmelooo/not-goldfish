//! `ng clear`: higiene procedural lossless disparada manualmente ("limpe meu
//! contexto agora"). Espelha o que o gate `PreCompact` do Claude Code faz de
//! forma automática (score → planejar eviction → stub-in-place com backup),
//! mas sob demanda a partir da CLI. Nada é perdido: cada item frio colapsado
//! vira um stub `[ng-evicted: ...]` recuperável e o conteúdo completo continua
//! no banco not-goldfish (recuperável com `ng search`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;
use ng_sessions::claude;
use ng_sessions::hygiene::{apply_eviction_claude, plan_eviction, score_items, EvictionPlan};
use ng_sessions::model::{SessionInfo, Transcript};

use crate::ui::Palette;

/// Quantos itens da prévia listar num `--dry-run` antes de resumir o resto.
const DRY_RUN_PREVIEW: usize = 5;

/// Ponto de entrada do subcomando. `file` sobrepõe a auto-detecção; quando
/// ausente, resolve o transcript mais recente do Claude Code para o cwd atual.
pub fn clear(file: Option<PathBuf>, target_tokens: i64, dry_run: bool) -> anyhow::Result<()> {
    let path = match file {
        Some(f) => f,
        None => resolve_latest_claude_transcript()?,
    };

    let transcript = parse_claude(&path)?;
    let scores = score_items(&transcript);
    let plan = plan_eviction(&scores, target_tokens);

    if plan.drops.is_empty() {
        let p = Palette::detect();
        println!(
            "Nada a colapsar: nenhum item frio acima do orçamento de {} tokens.",
            p.teal(target_tokens)
        );
        println!(
            "{}",
            p.dim(format!(
                "(Sessão com {} itens; hot zone e prompts do usuário são sempre preservados.)",
                transcript.items.len()
            ))
        );
        return Ok(());
    }

    if dry_run {
        print_dry_run(&transcript, &plan, target_tokens);
        return Ok(());
    }

    let result = apply_eviction_claude(&path, &transcript, &plan)
        .with_context(|| format!("aplicando higiene em {}", path.display()))?;
    let collapsed = plan.drops.len().saturating_sub(result.skipped);

    // Registra a economia líquida no gain_ledger — só depois do rename
    // atômico (apply_eviction_claude já trocou o arquivo) e só se algo foi
    // stubado de fato. Best-effort: métrica perdida é aceitável, falhar o
    // comando por causa dela não.
    if collapsed > 0 {
        record_gain(&transcript.info.id, &result, collapsed);
    }

    let p = Palette::detect();
    println!(
        "{} {}",
        p.ok("✓"),
        p.bold(format!("higiene aplicada em {}", path.display()))
    );
    println!("{}", p.kv("backup", p.dim(result.backup.display())));
    println!("{}", p.kv("itens colapsados", p.teal(collapsed)));
    println!(
        "{}",
        p.kv(
            "itens pulados",
            format!(
                "{} {}",
                p.teal(result.skipped),
                p.muted("(sem conteúdo seguro para stub)")
            )
        )
    );
    println!(
        "{}",
        p.kv(
            "tokens recuperados",
            p.teal(format!("~{}", plan.tokens_freed))
        )
    );
    println!();
    println!(
        "{}",
        p.dim("Retome a sessão do harness (--resume/--continue) para ver o contexto limpo.")
    );
    println!(
        "{}",
        p.dim("Nada foi perdido: cada item colapsado virou um stub [ng-evicted: ...] e o conteúdo")
    );
    println!(
        "{}",
        p.dim("completo continua no banco — recupere com `ng search`.")
    );
    Ok(())
}

/// Escrita best-effort de uma linha `kind='clear'` no `gain_ledger`:
/// economia líquida = tokens dos itens realmente stubados menos os tokens
/// dos próprios stubs (piso conservador do plano 003). Qualquer falha é
/// engolida — o gain é métrica, nunca motivo para o comando falhar; e nada
/// é escrito se o banco ainda nem existe (não criamos banco por efeito
/// colateral de uma métrica).
fn record_gain(session_id: &str, result: &ng_sessions::hygiene::EvictionApplyResult, items: usize) {
    let db = ng_core::paths::db_path();
    if !db.exists() {
        return;
    }
    let project = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let record = ng_core::GainRecord {
        kind: "clear".to_string(),
        session_id: session_id.to_string(),
        project,
        tokens: (result.tokens_evicted_est - result.stub_tokens_est).max(0),
        items: items as i64,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    if let Ok(store) = ng_core::Store::open(&db) {
        let _ = store.insert_gain(&record);
    }
}

/// Constrói um [`SessionInfo`] mínimo para `path` e o parseia como um
/// transcript do Claude Code. `claude::parse` só lê `info.path`, então os
/// demais campos são apenas metadados de conveniência.
fn parse_claude(path: &Path) -> anyhow::Result<Transcript> {
    let info = SessionInfo {
        id: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sessão".to_string()),
        harness: claude::HARNESS.to_string(),
        path: path.to_path_buf(),
        project: None,
        modified_at: SystemTime::now(),
        items_hint: None,
    };
    claude::parse(&info).with_context(|| format!("lendo transcript {}", path.display()))
}

/// Resumo pt-BR do que seria colapsado, sem tocar no arquivo.
fn print_dry_run(transcript: &Transcript, plan: &EvictionPlan, target_tokens: i64) {
    let p = Palette::detect();
    println!("{}", p.warn("DRY-RUN — nada será reescrito."));
    println!();
    println!(
        "{}",
        p.kv(
            "sessão",
            format!("{} itens", p.teal(transcript.items.len()))
        )
    );
    println!(
        "{}",
        p.kv(
            "orçamento de contexto",
            format!("{} tokens", p.teal(target_tokens))
        )
    );
    println!(
        "{}",
        p.kv("itens frios a colapsar", p.teal(plan.drops.len()))
    );
    println!(
        "{}",
        p.kv(
            "tokens a recuperar",
            p.teal(format!("~{}", plan.tokens_freed))
        )
    );
    println!();
    println!("Prévia dos primeiros itens que virariam stub:");

    let by_index: HashMap<usize, &_> = transcript.items.iter().map(|i| (i.index, i)).collect();
    for &idx in plan.drops.iter().take(DRY_RUN_PREVIEW) {
        if let Some(item) = by_index.get(&idx) {
            let preview: String = item.text_preview.chars().take(60).collect();
            println!(
                "  {} [{}] {} · {}",
                p.gold(format!("#{idx}")),
                p.teal(&item.kind),
                p.dim(format!("~{}tok", item.tokens_est)),
                preview.replace('\n', " ")
            );
        }
    }
    if plan.drops.len() > DRY_RUN_PREVIEW {
        println!(
            "{}",
            p.dim(format!(
                "  … e mais {} item(ns).",
                plan.drops.len() - DRY_RUN_PREVIEW
            ))
        );
    }
    println!();
    println!(
        "{}",
        p.dim("Rode sem --dry-run para aplicar (com backup automático).")
    );
}

/// Resolve o transcript `*.jsonl` mais recente do Claude Code para o
/// diretório de trabalho atual, em `~/.claude/projects/<cwd-mangado>/`.
fn resolve_latest_claude_transcript() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("lendo diretório de trabalho atual")?;
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("não foi possível localizar o diretório home"))?;
    let dir = home
        .join(".claude")
        .join("projects")
        .join(claude_project_dir_name(&cwd));

    newest_jsonl(&dir).ok_or_else(|| {
        anyhow::anyhow!(
            "nenhum transcript do Claude Code encontrado para este projeto em {}.\n\
             Passe --file <caminho.jsonl> apontando para o transcript a limpar.",
            dir.display()
        )
    })
}

/// Nome do diretório que o Claude Code usa em `~/.claude/projects` para um
/// cwd: o caminho absoluto com cada `/` e `.` trocados por `-` (confirmado
/// empiricamente — p.ex. `.claude` vira `-claude`).
fn claude_project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// `*.jsonl` mais recentemente modificado dentro de `dir`, por mtime.
/// `None` se o diretório não existe ou não contém nenhum `.jsonl` — nunca um
/// erro, já que a ausência é um estado esperado (projeto sem sessão ainda).
fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((p, mtime))
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn claude_project_dir_name_maps_slash_and_dot_to_dash() {
        assert_eq!(
            claude_project_dir_name(Path::new("/home/vitor/orca/projects/not-goldfish")),
            "-home-vitor-orca-projects-not-goldfish"
        );
        // Um `.` no caminho (ex.: um worktree sob `.claude`) também vira `-`,
        // produzindo o dash duplo que o Claude Code grava de verdade.
        assert_eq!(
            claude_project_dir_name(Path::new("/home/u/web/.claude/wt")),
            "-home-u-web--claude-wt"
        );
    }

    /// Define o mtime de `path` para um instante determinístico via `utimes`,
    /// para que o teste de "mais recente" não dependa da ordem de escrita.
    fn set_mtime(path: &Path, secs: i64) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let times = [
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
        ];
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes falhou ao ajustar mtime do fixture");
    }

    #[test]
    fn newest_jsonl_picks_most_recent_and_ignores_non_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let older = tmp.path().join("old.jsonl");
        let newer = tmp.path().join("new.jsonl");
        let decoy = tmp.path().join("notes.txt");
        std::fs::write(&older, "{}\n").unwrap();
        std::fs::write(&newer, "{}\n").unwrap();
        std::fs::write(&decoy, "irrelevante\n").unwrap();
        set_mtime(&older, 1_000_000);
        set_mtime(&newer, 2_000_000);
        // O .txt é mais novo que ambos, mas não deve ser considerado.
        set_mtime(&decoy, 9_000_000);

        assert_eq!(newest_jsonl(tmp.path()), Some(newer));
    }

    #[test]
    fn newest_jsonl_on_missing_dir_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(newest_jsonl(&tmp.path().join("inexistente")), None);
    }

    /// Transcript sintético mínimo: um prompt de usuário, um `tool_result`
    /// grande (candidato à eviction) e padding pequeno suficiente para
    /// empurrar o candidato para fora da hot zone (últimos 20 itens).
    fn write_min_transcript(dir: &Path) -> PathBuf {
        let path = dir.join("session.jsonl");
        let big = "x".repeat(600); // ~150 tokens estimados, acima do limiar de tool
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"Refatore o módulo de pagamento."}},"uuid":"u0"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}},"uuid":"u1","parentUuid":"a1"}}"#
        )
        .unwrap();
        for i in 2..25 {
            writeln!(
                f,
                r#"{{"type":"assistant","message":{{"role":"assistant","content":"ok {i}"}},"uuid":"pad{i}"}}"#
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn dry_run_does_not_modify_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_min_transcript(tmp.path());
        let before = std::fs::read(&path).unwrap();

        clear(Some(path.clone()), 4000, true).unwrap();

        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "dry-run não pode reescrever o transcript");
        // E nenhum backup deve ter sido criado ao lado.
        let backups: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("ng-bak"))
            .collect();
        assert!(backups.is_empty(), "dry-run não deve criar backup");
    }

    #[test]
    fn apply_collapses_candidate_and_writes_backup() {
        let tmp = tempfile::tempdir().unwrap();
        // Isola o gain_ledger num data dir descartável: o clear() real agora
        // registra a passada no banco, e um teste jamais pode escrever no
        // ~/.not-goldfish de quem roda a suíte.
        std::env::set_var("NG_DATA_DIR", tmp.path().join("ng-data"));
        // Pré-cria o banco (como `ng install`/o daemon fariam): record_gain
        // nunca cria banco por efeito colateral, só escreve num existente.
        drop(ng_core::Store::open(&ng_core::paths::db_path()).unwrap());
        let path = write_min_transcript(tmp.path());

        clear(Some(path.clone()), 4000, false).unwrap();

        // A passada ficou registrada no ledger do data dir isolado.
        let store = ng_core::Store::open_readonly(&ng_core::paths::db_path()).unwrap();
        let rows = store.gain_summary(None, None).unwrap();
        assert_eq!(rows.len(), 1);
        let (kind, runs, items, tokens) = &rows[0];
        assert_eq!(kind, "clear");
        assert_eq!((*runs, *items), (1, 1));
        assert!(
            *tokens > 0 && *tokens < 150,
            "economia líquida deve ser positiva e menor que o item bruto (~150tok), foi {tokens}"
        );

        let rewritten = std::fs::read_to_string(&path).unwrap();
        // O tool_result grande (linha 2) virou um stub recuperável.
        assert!(
            rewritten.contains("[ng-evicted:"),
            "o item frio deveria ter virado um stub"
        );
        // Um backup foi escrito ao lado do transcript.
        let has_backup = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("ng-bak"));
        assert!(has_backup, "aplicar higiene deve escrever um backup");
    }
}
