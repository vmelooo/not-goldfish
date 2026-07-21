//! `ng status`: estado do banco e do daemon, em texto humano ou JSON
//! estável para scripts.

use ng_core::{paths, Store};

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;

pub fn status(json: bool) -> anyhow::Result<()> {
    let db = paths::db_path();
    if json {
        // Contrato de scripts: mesmos dados do texto humano. `db: null`
        // quando o arquivo ainda não existe.
        let db_info = if db.exists() {
            let store = Store::open_readonly(&db)?;
            let (events, sessions, tokens) = store.stats()?;
            let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
            serde_json::json!({
                "exists": true,
                "events": events,
                "sessions": sessions,
                "tokens_est": tokens,
                "size_bytes": size,
            })
        } else {
            serde_json::Value::Null
        };
        let daemon_up = std::os::unix::net::UnixStream::connect(paths::socket_path()).is_ok();
        let out = serde_json::json!({
            "data_dir": paths::data_dir().display().to_string(),
            "db": db_info,
            "daemon_running": daemon_up,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let m = Msgs::get();
    let p = Palette::detect();
    println!("{}", p.banner(m.status_banner, ""));
    println!();
    println!("{}", p.kv(m.status_data, paths::data_dir().display()));
    if db.exists() {
        let store = Store::open_readonly(&db)?;
        let (events, sessions, tokens) = store.stats()?;
        let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
        println!(
            "{}",
            p.kv(
                m.status_db,
                fill(
                    m.status_db_fmt,
                    &[
                        ("{events}", &p.violet(events)),
                        ("{sessions}", &p.violet(sessions)),
                        ("{tokens}", &p.violet(tokens)),
                        ("{mib}", &format!("{:.1}", size as f64 / (1024.0 * 1024.0))),
                    ]
                )
            )
        );
    } else {
        println!("{}", p.kv(m.status_db, p.muted(m.status_db_absent)));
    }
    let daemon_up = std::os::unix::net::UnixStream::connect(paths::socket_path()).is_ok();
    println!(
        "{}",
        p.kv(
            m.status_daemon,
            if daemon_up {
                format!("{} {}", p.ok_glyph(), m.status_daemon_running)
            } else {
                format!("{} {}", p.warn_glyph(), p.muted(m.status_daemon_stopped))
            }
        )
    );
    Ok(())
}
