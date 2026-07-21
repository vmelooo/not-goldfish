//! `ng ui`: abre a UI web local (sobe o daemon em background se preciso).
//! O módulo chama-se `ui_cmd` para não colidir com o módulo `ui` do `ngd`.

use anyhow::Context;

use ng_core::paths;

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;
use crate::util;

/// Default must match `NG_UI_PORT`'s fallback in `ngd/src/ui.rs`.
const DEFAULT_UI_PORT: u16 = 4949;

pub fn ui() -> anyhow::Result<()> {
    let m = Msgs::get();
    let port: u16 = std::env::var("NG_UI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_UI_PORT);
    // Ensure a daemon is up (it serves the UI); spawn detached rather than
    // via `daemon()`'s exec-and-wait, since this command must return so the
    // browser can actually open.
    let socket = paths::socket_path();
    if std::os::unix::net::UnixStream::connect(&socket).is_err() {
        let ngd = util::find_sibling_binary("ngd").with_context(|| m.ngd_not_found)?;
        println!("{}", Palette::detect().dim(m.ui_starting_daemon));
        std::process::Command::new(&ngd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| fill(m.ui_starting, &[("{path}", &ngd.display())]))?;
        // Poll até a porta da UI responder em vez de um sleep cego: um
        // daemon que sobe rápido abre o browser na hora, e um que nunca
        // sobe vira um erro claro em vez de uma URL que dá 404.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100))
                .is_ok()
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("{}", m.ui_daemon_failed);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    let url = format!("http://127.0.0.1:{port}");
    println!("{}", Palette::detect().bold(&url));
    // Best-effort: a missing/failing xdg-open is not an error, the URL is
    // already printed above.
    let _ = std::process::Command::new("xdg-open").arg(&url).status();
    Ok(())
}
