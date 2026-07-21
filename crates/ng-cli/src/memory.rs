//! `ng memory`: inspeciona e edita a memória PRÓPRia do not-goldfish (o log
//! de eventos), espelhando os métodos de soft-state do `Store`. Ocultar é
//! sempre reversível e nada é apagado — ocultar só remove a memória da
//! busca/injeção; `unhide` a traz de volta.

use ng_core::{paths, timeutil, Store};

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;

/// Subcomandos de `ng memory`.
#[derive(Debug, clap::Subcommand)]
pub enum MemoryCommand {
    /// Lista memórias armazenadas (mais recentes primeiro)
    List {
        /// Limitar ao projeto atual
        #[arg(long)]
        here: bool,
        /// Incluir memórias ocultas (marcadas com [oculta])
        #[arg(long)]
        all: bool,
        /// Máximo de memórias
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Oculta uma memória da busca/injeção (reversível, nada é apagado)
    Hide {
        /// Id da memória (veja `ng memory list`)
        id: i64,
    },
    /// Restaura uma memória oculta para a busca/injeção
    Unhide {
        /// Id da memória
        id: i64,
    },
    /// Adiciona uma memória manualmente
    Add {
        /// Conteúdo da memória
        content: Vec<String>,
        /// Projeto ao qual associar (padrão: vazio = global)
        #[arg(long)]
        project: Option<String>,
        /// Tags (separadas por espaço)
        #[arg(long)]
        tags: Option<String>,
    },
}

pub fn memory(cmd: MemoryCommand) -> anyhow::Result<()> {
    let db = paths::db_path();
    // `add` may create a brand-new database (Store::open creates it), so it
    // works on a fresh install; the read/soft-state commands are meaningless
    // without captured data, so they bail with a friendly hint.
    if !db.exists() && !matches!(cmd, MemoryCommand::Add { .. }) {
        anyhow::bail!(
            "{}",
            fill(Msgs::get().db_missing, &[("{path}", &db.display())])
        );
    }
    match cmd {
        MemoryCommand::List { here, all, limit } => list(&db, here, all, limit),
        MemoryCommand::Hide { id } => hide(&db, id),
        MemoryCommand::Unhide { id } => unhide(&db, id),
        MemoryCommand::Add {
            content,
            project,
            tags,
        } => add(&db, &content.join(" "), project.as_deref(), tags.as_deref()),
    }
}

fn list(db: &std::path::Path, here: bool, all: bool, limit: usize) -> anyhow::Result<()> {
    let store = Store::open_readonly(db)?;
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let project = here.then_some(cwd.as_str());
    let memories = store.list_memories(project, all, limit)?;
    let msgs = Msgs::get();
    if memories.is_empty() {
        println!("{}", Palette::detect().muted(msgs.mem_empty));
        return Ok(());
    }
    let p = Palette::detect();
    for m in memories {
        let flags = format!(
            "{}{}",
            if m.hidden {
                p.warn(msgs.mem_flag_hidden)
            } else {
                String::new()
            },
            if m.manual {
                p.muted(msgs.mem_flag_manual)
            } else {
                String::new()
            }
        );
        println!(
            "#{} [{}] {} · {} · {} tok{}",
            p.gold(m.id),
            p.teal(&m.harness),
            m.kind,
            p.dim(timeutil::fmt_datetime(m.created_at)),
            m.tokens_est,
            flags
        );
        let preview: String = m.content.chars().take(120).collect();
        println!("   {}", preview.replace('\n', " "));
        if let Some(note) = m.note {
            println!("{}", p.dim(format!("   ✎ {note}")));
        }
    }
    Ok(())
}

// Mutações abrem o banco em RW (não readonly). WAL + busy_timeout do daemon
// tornam essa conexão de escrita de vida curta segura junto ao writer.
fn hide(db: &std::path::Path, id: i64) -> anyhow::Result<()> {
    let store = Store::open(db)?;
    let m = Msgs::get();
    let p = Palette::detect();
    if store.hide_memory(id)? {
        println!(
            "{}",
            fill(
                m.mem_hide_ok,
                &[
                    ("{id}", &p.gold(format!("#{id}"))),
                    ("{word}", &p.warn(m.mem_hidden_word)),
                    ("{raw}", &id),
                ]
            )
        );
    } else {
        println!(
            "{}",
            fill(m.mem_hide_none, &[("{id}", &p.gold(format!("#{id}")))])
        );
    }
    Ok(())
}

fn unhide(db: &std::path::Path, id: i64) -> anyhow::Result<()> {
    let store = Store::open(db)?;
    let m = Msgs::get();
    let p = Palette::detect();
    if store.unhide_memory(id)? {
        println!(
            "{}",
            fill(
                m.mem_unhide_ok,
                &[
                    ("{id}", &p.gold(format!("#{id}"))),
                    ("{word}", &p.ok(m.mem_unhide_word)),
                ]
            )
        );
    } else {
        println!(
            "{}",
            fill(m.mem_unhide_none, &[("{id}", &p.gold(format!("#{id}")))])
        );
    }
    Ok(())
}

fn add(
    db: &std::path::Path,
    content: &str,
    project: Option<&str>,
    tags: Option<&str>,
) -> anyhow::Result<()> {
    let m = Msgs::get();
    if content.trim().is_empty() {
        anyhow::bail!("{}", m.mem_add_empty);
    }
    let store = Store::open(db)?;
    let id = store.add_manual_memory(project.unwrap_or(""), content, tags.unwrap_or(""))?;
    let p = Palette::detect();
    println!(
        "{}",
        fill(
            m.mem_add_ok,
            &[
                ("{id}", &p.gold(format!("#{id}"))),
                ("{word}", &p.ok(m.mem_add_word)),
            ]
        )
    );
    Ok(())
}
