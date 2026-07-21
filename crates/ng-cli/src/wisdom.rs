//! `ng wisdom`: prints (or exports as Markdown) the co-occurrence "wisdom
//! graph" built from captured events — see `ng_core::graph`/`Store::neighbors`.

use ng_core::{paths, Store};

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;

pub fn wisdom(here: bool, md: bool, json: bool, rebuild: bool) -> anyhow::Result<()> {
    let m = Msgs::get();
    // O clap já impede a combinação via `conflicts_with`; este guard cobre
    // chamadas diretas à função.
    if json && md {
        anyhow::bail!("{}", m.wisdom_json_md_exclusive);
    }
    let db = paths::db_path();
    if !db.exists() {
        anyhow::bail!("{}", fill(m.db_missing, &[("{path}", &db.display())]));
    }
    if rebuild {
        // RW sem re-init: o schema já foi garantido pelo daemon no boot.
        let store = Store::open_rw_no_init(&db)?;
        println!("{}", Palette::detect().gold(m.wisdom_rebuilding));
        let processed = store.graph_rebuild()?;
        println!("{}", fill(m.wisdom_rebuilt, &[("{n}", &processed)]));
        return Ok(());
    }
    let store = Store::open_readonly(&db)?;
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let project = here.then_some(cwd.as_str());

    if md {
        let markdown = store.export_graph_md(project)?;
        print!("{markdown}");
        return Ok(());
    }

    // Top 5 heaviest entities in scope (graph_snapshot with no focus sorts
    // by weight descending), each with its own Store::neighbors — the
    // "who/what matters most, and what's near it" view.
    let (top_entities, _edges) = store.graph_snapshot(project, None, 0, 5)?;

    if json {
        // Contrato de scripts: nomes de campo são estáveis, mudanças futuras
        // devem ser aditivas. Grafo vazio = `"entities": []` com exit 0.
        let mut entities = Vec::with_capacity(top_entities.len());
        for entity in &top_entities {
            let neighbors = store.neighbors(&entity.name, project, 1, 5)?;
            entities.push(serde_json::json!({
                "name": entity.name,
                "kind": entity.kind,
                "weight": entity.weight,
                "neighbors": neighbors.iter().map(|(n, score)| serde_json::json!({
                    "name": n.name,
                    "kind": n.kind,
                    "weight": n.weight,
                    "score": score,
                })).collect::<Vec<_>>(),
            }));
        }
        let out = serde_json::json!({ "entities": entities });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let p = Palette::detect();
    let scope = if here {
        m.wisdom_scope_here
    } else {
        m.wisdom_scope_global
    };
    println!("{}", p.banner(m.wisdom_banner, scope));
    println!();
    if top_entities.is_empty() {
        println!("{}", p.muted(m.wisdom_empty));
        return Ok(());
    }

    for entity in &top_entities {
        println!(
            "{} {} [{}] {}",
            p.gold("●"),
            p.bold(&entity.name),
            p.violet(&entity.kind),
            p.muted(format!("{} {:.1}", m.wisdom_weight, entity.weight))
        );
        let neighbors = store.neighbors(&entity.name, project, 1, 5)?;
        if neighbors.is_empty() {
            println!("{}", p.muted(m.wisdom_no_neighbors));
            continue;
        }
        for (neighbor, score) in neighbors {
            println!(
                "   {} {} [{}] {}",
                p.violet("→"),
                neighbor.name,
                p.violet(&neighbor.kind),
                p.muted(format!("{} {:.2}", m.wisdom_score, score))
            );
        }
    }
    Ok(())
}
