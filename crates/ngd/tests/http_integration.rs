//! Real HTTP-level integration tests for `ngd::ui::router` (finding 18).
//!
//! Unlike `crates/ngd/src/ui.rs`'s own `#[cfg(test)]` unit tests (which
//! exercise pure functions like `path_is_allowed`/`stub_replacement_line`
//! directly), these drive the actual axum `Router` through
//! `tower::ServiceExt::oneshot` — real `Request`s in, real `Response`s out,
//! through the real middleware stack — so a regression in how the Host
//! guard, the token guard, and the handlers compose would show up here even
//! if every unit test still passed.
//!
//! `discover_sessions()` (via `ng_sessions`) and `ng_core::paths::data_dir()`
//! both read from process-wide state (`$HOME` / `$NG_DATA_DIR`), so every
//! test that depends on either is serialized through `ENV_GUARD` and given
//! its own tempdir — otherwise parallel `cargo test` threads would stomp on
//! each other's environment.

// `ENV_GUARD` is a `std::sync::Mutex` guarding process-global `$HOME`/
// `$NG_DATA_DIR` state, held for a whole test's duration — including every
// `.await` in it — on purpose: `discover_sessions()` reads those env vars
// synchronously deep inside the awaited request handling, so the lock must
// span the entire request lifecycle or two tests' env vars could interleave
// mid-request, which is precisely the flakiness this guard exists to
// prevent. Each `#[tokio::test]` runs on its own single-threaded runtime/OS
// thread with nothing else in the same test contending for this lock, so
// clippy's usual deadlock concern (another task on the same executor
// blocking on a lock held across a yield point) doesn't apply here.
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use ngd::ui::{router, SecurityState, UiState};

static ENV_GUARD: Mutex<()> = Mutex::new(());

const PORT: u16 = 4949;
const TOKEN: &str = "test-token-0123456789abcdef";

fn state() -> UiState {
    UiState::new(SecurityState {
        port: PORT,
        token: TOKEN.to_string(),
    })
}

/// O daemon garante o schema no boot (a writer thread roda `Store::open`
/// antes de a UI atender qualquer request); os testes que exercitam rotas de
/// escrita reproduzem isso, já que elas agora abrem com `open_rw_no_init` e
/// recusam um banco inexistente.
fn init_db() {
    drop(ng_core::Store::open(&ng_core::paths::db_path()).unwrap());
}

/// Points `$HOME` and `$NG_DATA_DIR` at fresh directories under `root` so
/// `discover_sessions()` and `ng_core::paths::*` never touch the real
/// developer machine's `~/.claude`, `~/.not-goldfish`, etc.
fn isolate_env(root: &Path) {
    std::env::set_var("HOME", root);
    std::env::set_var("NG_DATA_DIR", root.join("ng-data"));
}

/// Writes a minimal, valid claude-code transcript so `discover_sessions()`
/// finds it under the isolated `$HOME`. Returns its path.
fn write_claude_session(home: &Path, project: &str, session_id: &str) -> std::path::PathBuf {
    let dir = home.join(".claude").join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.jsonl"));
    let line = serde_json::json!({
        "type": "user",
        "uuid": "u1",
        "cwd": "/tmp/proj",
        "message": { "role": "user", "content": "ola, tudo bem?" },
    });
    std::fs::write(&path, format!("{}\n", line)).unwrap();
    path
}

async fn get(uri: &str, host: &str) -> axum::response::Response {
    let req = Request::builder()
        .uri(uri)
        .header(axum::http::header::HOST, host)
        .body(Body::empty())
        .unwrap();
    router(state()).oneshot(req).await.unwrap()
}

async fn post_json(
    uri: &str,
    host: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(axum::http::header::HOST, host)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        builder = builder.header("x-ng-token", t);
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    router(state()).oneshot(req).await.unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn ui_asset_pngs_are_served_with_png_content_type() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());

    for name in [
        "mascot",
        "empty-sessions",
        "empty-search",
        "empty-graph",
        "empty-memory",
        "swim-1",
        "swim-2",
        "swim-bubble",
    ] {
        let resp = get(&format!("/assets/ui/{name}.png"), "127.0.0.1:4949").await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "/assets/ui/{name}.png deve responder 200"
        );
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(ct, "image/png", "content-type de {name}.png");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            &bytes[..8],
            b"\x89PNG\r\n\x1a\n",
            "{name}.png deve carregar bytes PNG reais"
        );
    }
}

#[tokio::test]
async fn favicon_ico_is_served_as_png() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());

    let resp = get("/favicon.ico", "127.0.0.1:4949").await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/favicon.ico deve responder 200 (não 403)"
    );
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(
        ct, "image/png",
        "/favicon.ico deve ter content-type image/png"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        &bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "/favicon.ico deve carregar bytes PNG reais"
    );
}

#[tokio::test]
async fn foreign_host_header_is_rejected_on_every_route() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());

    for uri in ["/", "/api/sessions", "/api/status", "/api/graph"] {
        let resp = get(uri, "evil.example:4949").await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "expected 403 for {uri} with a foreign Host header"
        );
    }

    let resp = post_json(
        "/api/rewrite",
        "evil.example:4949",
        Some(TOKEN),
        serde_json::json!({"path": "/etc/passwd"}),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Host guard must reject mutating routes too, even with a valid token"
    );
}

#[tokio::test]
async fn allowed_host_forms_pass_the_guard() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());

    for host in ["127.0.0.1:4949", "localhost:4949", "[::1]:4949"] {
        let resp = get("/api/status", host).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "Host '{host}' should be allowed"
        );
    }
}

#[tokio::test]
async fn rewrite_without_token_is_rejected() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());

    let resp = post_json(
        "/api/rewrite",
        "127.0.0.1:4949",
        None,
        serde_json::json!({"path": "/etc/passwd"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn graph_bump_without_token_is_rejected_but_with_token_reaches_the_handler() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());
    init_db();

    let without = post_json(
        "/api/graph/bump",
        "127.0.0.1:4949",
        None,
        serde_json::json!({"name": "x", "delta": 0.5}),
    )
    .await;
    assert_eq!(without.status(), StatusCode::FORBIDDEN);

    let with = post_json(
        "/api/graph/bump",
        "127.0.0.1:4949",
        Some(TOKEN),
        serde_json::json!({"name": "x", "delta": 0.5}),
    )
    .await;
    assert_eq!(
        with.status(),
        StatusCode::OK,
        "a correct token must let the request reach the handler"
    );
}

#[tokio::test]
async fn transcript_for_a_real_discovered_session_succeeds() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());
    let session_path = write_claude_session(tmp.path(), "-tmp-proj", "sess-1");

    let uri = format!("/api/transcript?path={}", session_path.display());
    let resp = get(&uri, "127.0.0.1:4949").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(
        json.get("items").is_some(),
        "transcript response should carry parsed items"
    );
}

#[tokio::test]
async fn transcript_for_an_arbitrary_path_is_forbidden() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());
    // At least one real session must be discoverable so this isn't trivially
    // forbidden just because discover_sessions() returned nothing.
    write_claude_session(tmp.path(), "-tmp-proj", "sess-1");

    let resp = get("/api/transcript?path=/etc/passwd", "127.0.0.1:4949").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rewrite_happy_path_writes_a_backup_and_returns_it() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());
    let session_path = write_claude_session(tmp.path(), "-tmp-proj", "sess-1");

    let body = serde_json::json!({
        "path": session_path.display().to_string(),
        "drops": [],
        "replacements": [],
        "stubs": [],
    });
    let resp = post_json("/api/rewrite", "127.0.0.1:4949", Some(TOKEN), body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let backup = json["backup"]
        .as_str()
        .expect("response should carry a backup path");
    assert!(
        Path::new(backup).exists(),
        "the backup file the response points to must actually exist on disk"
    );
    assert!(
        session_path.exists(),
        "the original session file must be left in place"
    );
}

#[tokio::test]
async fn memory_add_then_list_roundtrips_through_the_real_router() {
    let _guard = ENV_GUARD.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_env(tmp.path());
    init_db();

    let add = post_json(
        "/api/memory/add",
        "127.0.0.1:4949",
        Some(TOKEN),
        serde_json::json!({"project": "/tmp/proj", "content": "memoria manual via ui", "tags": ""}),
    )
    .await;
    assert_eq!(add.status(), StatusCode::OK);
    let added = body_json(add).await;
    assert_eq!(added["ok"], true);

    let list = get("/api/memory", "127.0.0.1:4949").await;
    assert_eq!(list.status(), StatusCode::OK);
    let memories = body_json(list).await;
    let texts: Vec<&str> = memories
        .as_array()
        .expect("lista de memórias")
        .iter()
        .filter_map(|m| m["text"].as_str())
        .collect();
    assert!(
        texts.contains(&"memoria manual via ui"),
        "a memória adicionada deve aparecer na listagem: {texts:?}"
    );
}
