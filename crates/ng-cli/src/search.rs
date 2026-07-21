//! `ng search`: busca na memória persistente (FTS ou híbrida), com saída
//! humana citável ou JSON estável para scripts.

use ng_core::{paths, timeutil, HashEmbedder, Store};

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;

pub fn search(
    query: &str,
    here: bool,
    limit: usize,
    semantic: bool,
    json: bool,
) -> anyhow::Result<()> {
    let m = Msgs::get();
    let db = paths::db_path();
    if !db.exists() {
        anyhow::bail!("{}", fill(m.db_missing, &[("{path}", &db.display())]));
    }
    let store = Store::open_readonly(&db)?;
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let project = here.then_some(cwd.as_str());
    let hits = if semantic {
        store.search_hybrid(query, project, limit, &HashEmbedder)?
    } else {
        store.search(query, project, limit)?
    };
    if json {
        // Contrato de scripts: nomes de campo são estáveis, mudanças futuras
        // devem ser aditivas. Lista vazia = `"hits": []` com exit 0.
        let out = serde_json::json!({
            "query": query,
            "semantic": semantic,
            "hits": hits.iter().map(|h| serde_json::json!({
                "id": h.id,
                "session_id": h.session_id,
                "project": h.project,
                "harness": h.harness,
                "kind": h.kind,
                "snippet": h.snippet,
                "tags": h.tags,
                "created_at": h.created_at,
                "rank": h.rank,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let p = Palette::detect();
    println!("{}", p.banner(m.search_banner, &format!("— {query}")));
    println!();
    if hits.is_empty() {
        println!(
            "{}",
            p.muted(fill(m.search_no_results, &[("{query}", &query)]))
        );
        return Ok(());
    }
    for hit in hits {
        // Provenance line first — session, harness, timestamp — so results
        // are citable, not just findable. A linha SEMPRE começa com `#`
        // (scripts/e2e.sh conta resultados com `grep -c "^#"`); a cor entra
        // só depois dele e some por completo num pipe.
        println!(
            "#{} [{}] {} · {} {} · {}{}",
            p.gold(hit.id),
            p.violet(&hit.harness),
            hit.kind,
            m.search_session,
            // [finding 03] Byte-slicing here (`&s[..n]`) panics if a
            // Codex-derived session_id (comes straight from a filename, can
            // be non-ASCII) has a multibyte codepoint straddling the 8-byte
            // offset. Truncating by char is always safe.
            p.dim(truncate_chars(&hit.session_id, 8)),
            p.dim(timeutil::fmt_datetime(hit.created_at)),
            if semantic {
                p.dim(format!(" · score {:.3}", hit.rank))
            } else {
                String::new()
            },
        );
        // Snippet cru: os marcadores >>term<< do FTS já destacam o match e
        // qualquer pintura aqui poderia quebrar quem faz parse em pipe.
        println!("   {}", hit.snippet.replace('\n', " "));
    }
    Ok(())
}

/// First `n` `char`s of `s`, never splitting a multibyte codepoint —
/// unlike `&s[..n]`, which indexes by byte and panics on a non-ASCII
/// boundary.
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod truncate_chars_tests {
    use super::*;

    #[test]
    fn keeps_first_n_chars_of_an_ascii_string() {
        assert_eq!(truncate_chars("session-12345678-extra", 8), "session-");
    }

    #[test]
    fn shorter_than_n_is_returned_whole() {
        assert_eq!(truncate_chars("short", 8), "short");
    }

    #[test]
    fn does_not_panic_on_a_multibyte_boundary_at_the_cut_point() {
        // [finding 03] A Codex session_id comes straight from a filename
        // and can be non-ASCII; the old `&s[..8]` byte-slice would panic
        // here because "café-1234" has a 2-byte 'é' straddling byte
        // offset 8 (c-a-f-é(2 bytes)-1-2-3 = 8 bytes at the 7th char).
        let session_id = "café-12345678";
        let result = truncate_chars(session_id, 8);
        assert_eq!(result, "café-123");
        assert_eq!(result.chars().count(), 8);
    }

    #[test]
    fn works_on_a_string_that_is_almost_entirely_multibyte() {
        let session_id = "日本語のセッションID-1234";
        let result = truncate_chars(session_id, 8);
        assert_eq!(result.chars().count(), 8);
    }
}
