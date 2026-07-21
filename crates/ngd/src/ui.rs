//! Local-only web UI for visual context management, served by the daemon.
//!
//! Security posture: this server binds `127.0.0.1` ONLY, never `0.0.0.0` —
//! transcripts are raw session content (prompts, tool output, sometimes
//! secrets) and must never be reachable off the local machine. Every
//! filesystem path a request names is validated against the exact set of
//! paths [`ng_sessions::discover_sessions`] returned for this call; a local
//! HTTP server that opens whatever path a request supplies is a textbook
//! arbitrary file read/write primitive, so [`resolve_session`] is the single
//! chokepoint both the read (`/api/transcript`) and write (`/api/rewrite`)
//! endpoints go through. No other path is ever opened.
//!
//! Decisão CORRECTNESS-02: os handlers rodam SQLite bloqueante inline (sem
//! `spawn_blocking`) deliberadamente — UI local single-user em `127.0.0.1`,
//! contenção irrelevante. Se a UI um dia ganhar mais tráfego/fan-out de
//! requests, mover as chamadas de store para `spawn_blocking`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use ng_core::{paths, Entity, HashEmbedder, Memory, Store};
use ng_sessions::rewrite::rewrite_jsonl;
use ng_sessions::{discover_sessions, load_transcript, SessionInfo};

use crate::security;
use crate::stub::build_stub_replacements;

/// Matches the default `ng ui` prints in `ng-cli/src/main.rs` — keep both in
/// sync if this changes.
const DEFAULT_PORT: u16 = 4949;
const SESSION_LIMIT: usize = 100;
const SEARCH_LIMIT: usize = 20;
/// Max memories returned by `/api/memory` in one page — enough to browse the
/// recent history without streaming the whole event log into the UI.
const MEMORY_LIMIT: usize = 200;
/// Upper bound on a manually-added memory's content, matching the store's own
/// per-event cap so the UI rejects oversize input before it ever hits SQLite.
const MEMORY_MAX_CONTENT: usize = 256 * 1024;
/// Max entities rendered in the graph view — enough to be useful, small
/// enough for the SVG's no-simulation layout to stay readable and cheap.
const GRAPH_NODE_LIMIT: usize = 60;
const GRAPH_DEFAULT_DEPTH: usize = 2;
/// A single click's worth of promote/demote from the UI; large enough to
/// matter after a few clicks, small enough that one click never dominates
/// the graph (the store-side cap is the real backstop).
const GRAPH_BUMP_MAX: f64 = 5.0;

/// Shared state for the security middleware and the `index` handler (which
/// embeds the token into the served HTML). `port` is needed by the Host
/// allowlist since the allowed `Host` values are port-specific.
#[derive(Clone)]
pub struct SecurityState {
    pub port: u16,
    pub token: String,
}

/// Estado completo do router: os guards de segurança + a conexão read-only
/// compartilhada pelos handlers GET. `Mutex` porque `rusqlite::Connection`
/// não é `Sync`; contenção é irrelevante (UI local single-user). O `Option`
/// permite abrir lazily no primeiro uso — na subida da UI o banco pode ainda
/// não existir (o daemon o cria no boot, em outra thread).
#[derive(Clone)]
pub struct UiState {
    pub security: SecurityState,
    pub store_ro: std::sync::Arc<std::sync::Mutex<Option<Store>>>,
}

impl UiState {
    pub fn new(security: SecurityState) -> Self {
        UiState {
            security,
            store_ro: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

/// Permite que os handlers que só precisam de segurança (ex.: [`index`])
/// continuem extraindo `State<SecurityState>` de um router com [`UiState`].
impl axum::extract::FromRef<UiState> for SecurityState {
    fn from_ref(state: &UiState) -> Self {
        state.security.clone()
    }
}

/// Trava a conexão read-only compartilhada, abrindo-a no primeiro uso.
/// Falha de abertura devolve o mesmo erro por request de antes (e deixa o
/// slot vazio, então o próximo request tenta de novo). O guard devolvido é
/// garantidamente `Some`. Erro em `Box` pela mesma razão de [`open_rw`]
/// (clippy `result_large_err`).
fn lock_store_ro(
    state: &UiState,
) -> Result<std::sync::MutexGuard<'_, Option<Store>>, Box<axum::response::Response>> {
    let mut guard = match state.store_ro.lock() {
        Ok(guard) => guard,
        Err(err) => {
            return Err(Box::new(log_and_generic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "erro abrindo banco",
                err,
            )))
        }
    };
    if guard.is_none() {
        match Store::open_readonly(&paths::db_path()) {
            Ok(store) => *guard = Some(store),
            Err(err) => {
                return Err(Box::new(log_and_generic_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "erro abrindo banco",
                    err,
                )))
            }
        }
    }
    Ok(guard)
}

/// Starts the UI server and blocks forever. Called from its own
/// `std::thread` with its own tokio runtime (see `main.rs`) — the daemon's
/// socket accept loop stays std-only, unaffected by this dependency.
pub fn run() {
    let port: u16 = std::env::var("NG_UI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let token = security::generate_token();
    eprintln!(
        "ngd: ui: session token generated (required as X-NG-Token on state-changing requests)"
    );

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("ngd: ui: cannot build tokio runtime: {err}");
            return;
        }
    };
    rt.block_on(serve(port, UiState::new(SecurityState { port, token })));
}

async fn serve(port: u16, state: UiState) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("ngd: ui: cannot bind {addr}: {err}");
            return;
        }
    };
    eprintln!("ngd: ui listening on http://{addr}");
    if let Err(err) = axum::serve(listener, router(state)).await {
        eprintln!("ngd: ui: server error: {err}");
    }
}

/// Builds the full router: state-changing routes (`/api/rewrite`,
/// `/api/graph/bump`) require `X-NG-Token`; every route — including those
/// two — additionally requires an allowed `Host` header. The Host guard is
/// applied last (outermost), so it runs *first* on every incoming request,
/// before the token check ever sees it: a DNS-rebinding attempt gets
/// rejected before it can even probe whether a token is required.
pub fn router(state: UiState) -> Router {
    let security = state.security.clone();
    let mutating = Router::new()
        .route("/api/rewrite", post(api_rewrite))
        .route("/api/graph/bump", post(api_graph_bump))
        .route("/api/memory/hide", post(api_memory_hide))
        .route("/api/memory/unhide", post(api_memory_unhide))
        .route("/api/memory/annotate", post(api_memory_annotate))
        .route("/api/memory/edit", post(api_memory_edit))
        .route("/api/memory/add", post(api_memory_add))
        .layer(middleware::from_fn_with_state(
            security.clone(),
            require_token,
        ));

    Router::new()
        .route("/", get(index))
        .route("/assets/goldfish-not.png", get(goldfish_logo))
        .route(
            "/assets/ui/mascot.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/mascot.png")) }),
        )
        // Browsers auto-request /favicon.ico; serve the mascot PNG bytes so it
        // is a 200 (modern browsers accept PNG at .ico) instead of a 403 from
        // the fall-through. Same embedded bytes as /assets/ui/mascot.png.
        .route(
            "/favicon.ico",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/mascot.png")) }),
        )
        .route(
            "/assets/ui/empty-sessions.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/empty-sessions.png")) }),
        )
        .route(
            "/assets/ui/empty-search.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/empty-search.png")) }),
        )
        .route(
            "/assets/ui/empty-graph.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/empty-graph.png")) }),
        )
        .route(
            "/assets/ui/empty-memory.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/empty-memory.png")) }),
        )
        .route(
            "/assets/ui/swim-1.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/swim-1.png")) }),
        )
        .route(
            "/assets/ui/swim-2.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/swim-2.png")) }),
        )
        .route(
            "/assets/ui/swim-bubble.png",
            get(|| async { ui_png(include_bytes!("../../../assets/ui/swim-bubble.png")) }),
        )
        .route("/api/sessions", get(api_sessions))
        .route("/api/transcript", get(api_transcript))
        .route("/api/transcript/item", get(api_transcript_item))
        .route("/api/search", get(api_search))
        .route("/api/status", get(api_status))
        .route("/api/graph", get(api_graph))
        .route("/api/graph.md", get(api_graph_md))
        .route("/api/memory", get(api_memory))
        .merge(mutating)
        .layer(middleware::from_fn_with_state(
            security,
            require_allowed_host,
        ))
        .with_state(state)
}

/// Sprites e ilustrações da camada lúdica (`assets/ui/*.png`), embutidos no
/// binário como o logo — mesmos headers, nenhum acesso a disco em runtime.
fn ui_png(bytes: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    )
}

async fn goldfish_logo() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("../../../assets/readme/goldfish-not.png").as_slice(),
    )
}

/// DNS-rebinding guard: rejects any request whose `Host` header isn't in
/// the loopback allowlist for this server's port. See `security` module
/// docs for why this — not the `127.0.0.1` bind alone — is what actually
/// stops rebinding.
async fn require_allowed_host(
    State(security): State<SecurityState>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !security::host_is_allowed(host, security.port) {
        eprintln!("ngd: ui: rejected request with Host '{host}' (DNS-rebinding guard)");
        return (StatusCode::FORBIDDEN, "host não permitido").into_response();
    }
    next.run(req).await
}

/// CSRF-style guard for state-changing routes: requires `X-NG-Token` to
/// match the token generated at boot and embedded in the served HTML (see
/// [`render_index_html`]).
async fn require_token(
    State(security): State<SecurityState>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let provided = req
        .headers()
        .get("x-ng-token")
        .and_then(|v| v.to_str().ok());
    if !security::token_matches(&security.token, provided) {
        return (
            StatusCode::FORBIDDEN,
            "token ausente ou inválido — recarregue a página da UI",
        )
            .into_response();
    }
    next.run(req).await
}

async fn index(State(security): State<SecurityState>) -> impl IntoResponse {
    // no-cache: a página muda a cada build do daemon e uma versão velha em
    // cache do navegador parece "funcionar" mas com bugs já corrigidos —
    // revalida sempre (o HTML é pequeno e servido localmente).
    (
        [(
            axum::http::header::CACHE_CONTROL,
            "no-cache, must-revalidate",
        )],
        Html(render_index_html(&security.token)),
    )
}

/// Embeds the boot-time session token into the served HTML as a JS
/// constant the frontend reads before making any state-changing request.
/// The token is always a hex string this process generated itself (never
/// user input), so no escaping concern — the substitution target is a
/// fixed placeholder comment that can't collide with anything else in the
/// template.
fn render_index_html(token: &str) -> String {
    const TEMPLATE: &str = include_str!("ui.html");
    TEMPLATE.replacen(
        "/*NG_TOKEN_PLACEHOLDER*/",
        &format!("const NG_TOKEN = \"{token}\";"),
        1,
    )
}

/// Logs the real error server-side (stderr, operator-visible in the
/// daemon's own output) and returns a generic message to the HTTP client.
/// Full error text can carry local file paths, SQLite internals, or other
/// host details that must never leave the machine in a response body.
fn log_and_generic_error(
    status: StatusCode,
    public_message: &'static str,
    err: impl std::fmt::Display,
) -> axum::response::Response {
    eprintln!("ngd: ui: {public_message}: {err}");
    (status, public_message).into_response()
}

async fn api_sessions() -> impl IntoResponse {
    let mut sessions = discover_sessions();
    sessions.truncate(SESSION_LIMIT);
    Json(sessions)
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

/// `/api/transcript` response: o transcript + o modelo detectado da sessão
/// (hint para a UI dimensionar a janela de contexto — nunca um dado rígido).
#[derive(Serialize)]
struct TranscriptResponse {
    model: Option<String>,
    #[serde(flatten)]
    transcript: ng_sessions::Transcript,
}

async fn api_transcript(Query(q): Query<PathQuery>) -> axum::response::Response {
    let sessions = discover_sessions();
    let info = match resolve_session(&q.path, &sessions) {
        Ok(info) => info,
        Err(msg) => return (StatusCode::FORBIDDEN, msg).into_response(),
    };
    let model = ng_sessions::detect_model(info);
    match load_transcript(info) {
        Ok(transcript) => Json(TranscriptResponse { model, transcript }).into_response(),
        Err(err) => log_and_generic_error(StatusCode::BAD_REQUEST, "erro ao ler transcript", err),
    }
}

#[derive(Deserialize)]
struct TranscriptItemQuery {
    path: String,
    line: usize,
}

/// `/api/transcript/item?path&line` — o conteúdo COMPLETO de um único item
/// (o listado do transcript vai só com previews de 200 chars; o detalhe
/// completo vem sob demanda aqui, sob o mesmo chokepoint de segurança
/// [`resolve_session`]). Usado pelo drawer de detalhe/edição manual.
async fn api_transcript_item(Query(q): Query<TranscriptItemQuery>) -> axum::response::Response {
    let sessions = discover_sessions();
    let info = match resolve_session(&q.path, &sessions) {
        Ok(info) => info,
        Err(msg) => return (StatusCode::FORBIDDEN, msg).into_response(),
    };
    let transcript = match load_transcript(info) {
        Ok(t) => t,
        Err(err) => {
            return log_and_generic_error(StatusCode::BAD_REQUEST, "erro ao ler transcript", err)
        }
    };
    let Some(item) = transcript.items.iter().find(|i| i.raw_line == Some(q.line)) else {
        return (StatusCode::NOT_FOUND, "item não encontrado nessa linha").into_response();
    };
    Json(serde_json::json!({
        "line": q.line,
        "role": item.role,
        "kind": item.kind,
        "tokens_est": item.tokens_est,
        "text": item.text_full,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RewriteRequest {
    path: String,
    #[serde(default)]
    drops: Vec<usize>,
    /// `(1-based line number, replacement JSON)` pairs, same addressing as
    /// [`ng_sessions::model::SessionItem::raw_line`].
    #[serde(default)]
    replacements: Vec<(usize, String)>,
    /// Line numbers whose content should be swapped for an eviction stub.
    /// The backend builds the replacement JSON (see
    /// [`crate::stub::stub_replacement_line`])
    /// so the frontend never needs to reconstruct harness-specific shapes.
    #[serde(default)]
    stubs: Vec<usize>,
    /// `(1-based line number, new content text)` — manual edits. Same
    /// discipline as `stubs`: the backend rebuilds the line JSON with the
    /// new content (see [`crate::stub::edit_replacement_line`]); a line
    /// can't be edited AND dropped/stubbed in the same request.
    #[serde(default)]
    edits: Vec<(usize, String)>,
}

#[derive(Serialize)]
struct RewriteResponse {
    backup: String,
}

async fn api_rewrite(Json(req): Json<RewriteRequest>) -> axum::response::Response {
    let sessions = discover_sessions();
    let info = match resolve_session(&req.path, &sessions) {
        Ok(info) => info,
        Err(msg) => return (StatusCode::FORBIDDEN, msg).into_response(),
    };
    // Only line-addressable formats can be safely rewritten (see
    // ng_sessions::rewrite's own doc comment); opencode/gemini store one
    // JSON document or one file per message and have no `raw_line`.
    if !matches!(info.harness.as_str(), "claude-code" | "codex" | "kimi") {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "harness '{}' não suporta edição por linha (apenas claude/codex/kimi)",
                info.harness
            ),
        )
            .into_response();
    }

    let mut replacements = req.replacements.clone();
    // Edições manuais: teto de tamanho por texto e nenhum conflito com
    // drops/stubs na mesma linha (uma linha não pode ser editada E removida
    // E stubada no mesmo pedido — seria ambíguo por definição).
    const MAX_EDIT_TEXT_BYTES: usize = 256 * 1024;
    for (line, text) in &req.edits {
        if text.len() > MAX_EDIT_TEXT_BYTES {
            return (
                StatusCode::BAD_REQUEST,
                format!("edição da linha {line} excede 256 KiB"),
            )
                .into_response();
        }
        if req.drops.contains(line) {
            return (
                StatusCode::BAD_REQUEST,
                format!("linha {line} não pode ser editada E removida no mesmo pedido"),
            )
                .into_response();
        }
        if req.stubs.contains(line) {
            return (
                StatusCode::BAD_REQUEST,
                format!("linha {line} não pode ser editada E stubada no mesmo pedido"),
            )
                .into_response();
        }
    }
    if !req.stubs.is_empty() || !req.edits.is_empty() {
        // [finding 19b] This read is only to *derive* the stub replacement
        // JSON from each target line's current content; `rewrite_jsonl` reads
        // the file again for the actual swap. The two reads used to disagree
        // on line boundaries — this handler split with `str::lines()` (drops a
        // trailing `\r`) while `rewrite_jsonl` uses the canonical
        // `split_lines` (keeps it) — so on a CRLF transcript a stub could be
        // built from one line but applied to another. Both paths now go
        // through `split_lines`, so line targeting can never diverge.
        // Residual: since `rewrite_jsonl` re-reads, a concurrent harness
        // *append* between the two reads is still possible, but an append only
        // adds lines at the end and never shifts the 1-based numbers of the
        // lines being stubbed, so it cannot cause the wrong line to be hit.
        let original = match std::fs::read_to_string(&info.path) {
            Ok(s) => s,
            Err(err) => {
                return log_and_generic_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "erro lendo sessão",
                    err,
                )
            }
        };
        if !req.stubs.is_empty() {
            match build_stub_replacements(&original, &req.stubs, &info.harness) {
                Ok(mut stub_replacements) => replacements.append(&mut stub_replacements),
                Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
            }
        }
        if !req.edits.is_empty() {
            match crate::stub::build_edit_replacements(&original, &req.edits, &info.harness) {
                Ok(mut edit_replacements) => replacements.append(&mut edit_replacements),
                Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
            }
        }
    }

    match rewrite_jsonl(&info.path, &req.drops, &replacements) {
        Ok(backup) => Json(RewriteResponse {
            backup: backup.display().to_string(),
        })
        .into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::BAD_REQUEST,
            "rewrite falhou (arquivo original intocado)",
            err,
        ),
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn api_search(
    State(state): State<UiState>,
    Query(q): Query<SearchQuery>,
) -> axum::response::Response {
    if !paths::db_path().exists() {
        return Json(Vec::<ng_core::SearchHit>::new()).into_response();
    }
    let guard = match lock_store_ro(&state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };
    let store = guard.as_ref().expect("lock_store_ro garante Some");
    match store.search_hybrid(&q.q, None, SEARCH_LIMIT, &HashEmbedder) {
        Ok(hits) => Json(hits).into_response(),
        Err(err) => log_and_generic_error(StatusCode::INTERNAL_SERVER_ERROR, "erro na busca", err),
    }
}

#[derive(Serialize)]
struct StatusResponse {
    version: String,
    events: i64,
    sessions: i64,
    tokens_est: i64,
}

async fn api_status(State(state): State<UiState>) -> axum::response::Response {
    if !paths::db_path().exists() {
        return Json(StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            events: 0,
            sessions: 0,
            tokens_est: 0,
        })
        .into_response();
    }
    let guard = match lock_store_ro(&state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };
    let store = guard.as_ref().expect("lock_store_ro garante Some");
    match store.stats() {
        Ok((events, sessions, tokens_est)) => Json(StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            events,
            sessions,
            tokens_est,
        })
        .into_response(),
        Err(err) => {
            log_and_generic_error(StatusCode::INTERNAL_SERVER_ERROR, "erro lendo stats", err)
        }
    }
}

#[derive(Deserialize)]
struct GraphQuery {
    project: Option<String>,
    focus: Option<String>,
    #[serde(default = "default_graph_depth")]
    depth: usize,
}

fn default_graph_depth() -> usize {
    GRAPH_DEFAULT_DEPTH
}

#[derive(Serialize, PartialEq, Debug)]
struct GraphNode {
    name: String,
    kind: String,
    weight: f64,
}

#[derive(Serialize, PartialEq, Debug)]
struct GraphEdge {
    a: String,
    b: String,
    weight: f64,
}

#[derive(Serialize, PartialEq, Debug)]
struct GraphResponse {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
}

async fn api_graph(
    State(state): State<UiState>,
    Query(q): Query<GraphQuery>,
) -> axum::response::Response {
    if !paths::db_path().exists() {
        return Json(GraphResponse {
            nodes: Vec::new(),
            edges: Vec::new(),
        })
        .into_response();
    }
    let guard = match lock_store_ro(&state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };
    let store = guard.as_ref().expect("lock_store_ro garante Some");
    match store.graph_snapshot(
        q.project.as_deref(),
        q.focus.as_deref(),
        q.depth,
        GRAPH_NODE_LIMIT,
    ) {
        Ok((entities, edges)) => Json(build_graph_response(&entities, &edges)).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro montando grafo",
            err,
        ),
    }
}

/// Converts store-side (entity-id) edges into the name-addressed wire
/// format the frontend consumes. Entity names are expected to be unique
/// within one response in practice — file names always contain '/',
/// errors/concepts are short lexical tokens, decisions are full sentences,
/// so exact cross-kind collisions are exceedingly rare — but this is
/// defensive either way: an edge whose endpoint id isn't in `entities` is
/// dropped rather than panicking or emitting a bogus empty name.
fn build_graph_response(entities: &[Entity], edges: &[(i64, i64, f64)]) -> GraphResponse {
    let by_id: std::collections::HashMap<i64, &Entity> =
        entities.iter().map(|e| (e.id, e)).collect();
    let nodes = entities
        .iter()
        .map(|e| GraphNode {
            name: e.name.clone(),
            kind: e.kind.clone(),
            weight: e.weight,
        })
        .collect();
    let edges = edges
        .iter()
        .filter_map(|&(a, b, weight)| {
            let a = by_id.get(&a)?;
            let b = by_id.get(&b)?;
            Some(GraphEdge {
                a: a.name.clone(),
                b: b.name.clone(),
                weight,
            })
        })
        .collect();
    GraphResponse { nodes, edges }
}

#[derive(Deserialize)]
struct GraphMdQuery {
    project: Option<String>,
}

async fn api_graph_md(
    State(state): State<UiState>,
    Query(q): Query<GraphMdQuery>,
) -> axum::response::Response {
    let plain_text = [(
        axum::http::header::CONTENT_TYPE,
        "text/plain; charset=utf-8",
    )];
    if !paths::db_path().exists() {
        return (StatusCode::OK, plain_text, String::new()).into_response();
    }
    let guard = match lock_store_ro(&state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };
    let store = guard.as_ref().expect("lock_store_ro garante Some");
    match store.export_graph_md(q.project.as_deref()) {
        Ok(md) => (StatusCode::OK, plain_text, md).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro exportando grafo",
            err,
        ),
    }
}

#[derive(Deserialize)]
struct BumpRequest {
    name: String,
    delta: f64,
}

#[derive(Serialize)]
struct BumpResponse {
    updated: usize,
}

/// User-driven feedback on graph weight ("this suggestion was useful" /
/// "stop surfacing this") must stay a nudge, not an injection vector or a
/// way to corrupt the graph — reject blank names and non-finite/oversized
/// deltas before it ever reaches [`Store::bump_entity`] (whose own clamp
/// to `[0, MAX_GRAPH_WEIGHT]` is the last line of defense, not the only one).
fn validate_bump(req: &BumpRequest) -> Result<(), String> {
    if req.name.trim().is_empty() {
        return Err("nome vazio".to_string());
    }
    if !req.delta.is_finite() {
        return Err("delta inválido (deve ser finito)".to_string());
    }
    if req.delta.abs() > GRAPH_BUMP_MAX {
        return Err(format!(
            "delta fora do intervalo permitido (±{GRAPH_BUMP_MAX})"
        ));
    }
    Ok(())
}

async fn api_graph_bump(Json(req): Json<BumpRequest>) -> axum::response::Response {
    if let Err(msg) = validate_bump(&req) {
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }
    // RW open (not readonly): this endpoint writes to the graph tables
    // directly; WAL + the daemon's busy_timeout make it safe alongside the
    // writer thread and the enrichment worker. `open_rw_no_init` porque o
    // schema já foi garantido pelo boot do daemon — sem re-DDL nem retry de
    // cold start num clique do usuário.
    let store = match Store::open_rw_no_init(&paths::db_path()) {
        Ok(s) => s,
        Err(err) => {
            return log_and_generic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "erro abrindo banco",
                err,
            )
        }
    };
    match store.bump_entity(&req.name, req.delta) {
        Ok(updated) => Json(BumpResponse { updated }).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro atualizando peso",
            err,
        ),
    }
}

// ---- Memória: view/hide/restore/annotate/add own captured memory ----------
//
// These endpoints edit not-goldfish's OWN stored memory (the events log),
// never the harness transcript that /api/rewrite touches. The core invariant
// holds: hide is a soft mask (sets `hidden_at`, dropping the row from
// search/injection) and is fully reversible; content is only ever rewritten
// in place for *manual* memories (`manual = 1`) — captured events are never
// mutated or physically DELETEd. Editing a manual memory drops that row's
// derived embedding so the worker recomputes it; that is a cache, not the
// source-of-truth event. Reads go through a read-only connection; the five
// mutating routes sit behind the same X-NG-Token + Host guards as /api/rewrite.

#[derive(Deserialize)]
struct MemoryQuery {
    project: Option<String>,
    scope: Option<String>,
}

/// Wire shape for one memory. Field names match the tolerant reader in
/// `ui.html` (`m.kind`/`m.project`/`m.tokens_est`/`m.created_at`/`m.text`),
/// plus the soft-state fields the block view needs (`hidden`, `note`,
/// `manual`).
#[derive(Serialize)]
struct MemoryDto {
    id: i64,
    project: String,
    harness: String,
    kind: String,
    text: String,
    tags: String,
    tokens_est: i64,
    created_at: i64,
    hidden: bool,
    note: Option<String>,
    manual: bool,
}

impl From<Memory> for MemoryDto {
    fn from(m: Memory) -> Self {
        MemoryDto {
            id: m.id,
            project: m.project,
            harness: m.harness,
            kind: m.kind,
            text: m.content,
            tags: m.tags,
            tokens_est: m.tokens_est,
            created_at: m.created_at,
            hidden: m.hidden,
            note: m.note,
            manual: m.manual,
        }
    }
}

/// Resolve the `scope`/`project` query pair into the project filter passed to
/// [`Store::list_memories`]: `scope=project` with a non-blank project name
/// restricts to that project; anything else (including `scope=global` or a
/// blank name) lists globally. Pure so the routing choice is unit-testable.
fn resolve_memory_scope<'a>(scope: Option<&str>, project: Option<&'a str>) -> Option<&'a str> {
    match scope {
        Some("project") => match project {
            Some(p) if !p.trim().is_empty() => Some(p),
            _ => None,
        },
        _ => None,
    }
}

async fn api_memory(
    State(state): State<UiState>,
    Query(q): Query<MemoryQuery>,
) -> axum::response::Response {
    if !paths::db_path().exists() {
        return Json(Vec::<MemoryDto>::new()).into_response();
    }
    let guard = match lock_store_ro(&state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };
    let store = guard.as_ref().expect("lock_store_ro garante Some");
    let project = resolve_memory_scope(q.scope.as_deref(), q.project.as_deref());
    // include_hidden = true: the view must surface hidden memories (clearly
    // flagged) so the user can restore them — hiding is reversible by design.
    match store.list_memories(project, true, MEMORY_LIMIT) {
        Ok(memories) => {
            let dtos: Vec<MemoryDto> = memories.into_iter().map(MemoryDto::from).collect();
            Json(dtos).into_response()
        }
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro listando memórias",
            err,
        ),
    }
}

#[derive(Deserialize)]
struct MemoryIdRequest {
    id: i64,
}

#[derive(Deserialize)]
struct AnnotateRequest {
    id: i64,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
struct EditMemoryRequest {
    id: i64,
    content: String,
    #[serde(default)]
    tags: String,
}

#[derive(Deserialize)]
struct AddMemoryRequest {
    #[serde(default)]
    project: String,
    content: String,
    #[serde(default)]
    tags: String,
}

#[derive(Serialize)]
struct MemoryMutationResponse {
    /// Whether a row was affected (false = unknown id / no-op).
    ok: bool,
}

/// RW open shared by every mutating memory route — same rationale as
/// [`api_graph_bump`]: WAL + the daemon's busy_timeout make a short-lived
/// writer connection safe alongside the writer thread and enrichment worker,
/// and `open_rw_no_init` pula o re-DDL/retry porque o boot do daemon já
/// garantiu o schema.
/// The error variant is boxed because an `axum::response::Response` is large;
/// returning it unboxed trips clippy's `result_large_err` on every caller.
fn open_rw() -> Result<Store, Box<axum::response::Response>> {
    Store::open_rw_no_init(&paths::db_path()).map_err(|err| {
        Box::new(log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro abrindo banco",
            err,
        ))
    })
}

async fn api_memory_hide(Json(req): Json<MemoryIdRequest>) -> axum::response::Response {
    let store = match open_rw() {
        Ok(s) => s,
        Err(resp) => return *resp,
    };
    match store.hide_memory(req.id) {
        Ok(ok) => Json(MemoryMutationResponse { ok }).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro ocultando memória",
            err,
        ),
    }
}

async fn api_memory_unhide(Json(req): Json<MemoryIdRequest>) -> axum::response::Response {
    let store = match open_rw() {
        Ok(s) => s,
        Err(resp) => return *resp,
    };
    match store.unhide_memory(req.id) {
        Ok(ok) => Json(MemoryMutationResponse { ok }).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro restaurando memória",
            err,
        ),
    }
}

async fn api_memory_annotate(Json(req): Json<AnnotateRequest>) -> axum::response::Response {
    let store = match open_rw() {
        Ok(s) => s,
        Err(resp) => return *resp,
    };
    match store.annotate_memory(req.id, &req.note) {
        Ok(ok) => Json(MemoryMutationResponse { ok }).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro anotando memória",
            err,
        ),
    }
}

/// Edit the content of a MANUAL memory. Captured memories are read-only by
/// design — the store refuses them (`ok: false`), keeping the "nothing
/// captured is mutated" invariant on the server side too, not just the UI.
async fn api_memory_edit(Json(req): Json<EditMemoryRequest>) -> axum::response::Response {
    let content = match validate_manual_memory(&req.content) {
        Ok(c) => c,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let store = match open_rw() {
        Ok(s) => s,
        Err(resp) => return *resp,
    };
    match store.edit_memory_content(req.id, content, req.tags.trim()) {
        Ok(ok) => Json(MemoryMutationResponse { ok }).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro editando memória",
            err,
        ),
    }
}

/// Validate a manual-add payload before it touches the store: non-blank
/// content within the storage cap. Returns the trimmed content on success.
fn validate_manual_memory(content: &str) -> Result<&str, String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("conteúdo vazio".to_string());
    }
    if content.len() > MEMORY_MAX_CONTENT {
        return Err("conteúdo excede o limite".to_string());
    }
    Ok(trimmed)
}

async fn api_memory_add(Json(req): Json<AddMemoryRequest>) -> axum::response::Response {
    let content = match validate_manual_memory(&req.content) {
        Ok(c) => c,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let store = match open_rw() {
        Ok(s) => s,
        Err(resp) => return *resp,
    };
    match store.add_manual_memory(req.project.trim(), content, req.tags.trim()) {
        Ok(id) => Json(serde_json::json!({ "ok": true, "id": id })).into_response(),
        Err(err) => log_and_generic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erro adicionando memória",
            err,
        ),
    }
}

/// Core of the path-traversal guard: `candidate` must be byte-equal to one
/// of `allowed` (both already canonicalized by the caller — canonicalizing
/// resolves symlinks/`..` so string comparison alone can't be tricked).
fn path_is_allowed(candidate: &Path, allowed: &[PathBuf]) -> bool {
    allowed.iter().any(|p| p == candidate)
}

/// Resolves a request's `path` string to one of the sessions
/// `discover_sessions()` actually returned, or refuses it. This is the only
/// place a request-supplied path is turned into an open/read/write — every
/// handler that touches disk goes through here first.
fn resolve_session<'a>(
    requested: &str,
    sessions: &'a [SessionInfo],
) -> Result<&'a SessionInfo, String> {
    let canon = Path::new(requested)
        .canonicalize()
        .map_err(|_| "caminho inválido ou inexistente".to_string())?;
    let allowed: Vec<PathBuf> = sessions
        .iter()
        .filter_map(|s| s.path.canonicalize().ok())
        .collect();
    if !path_is_allowed(&canon, &allowed) {
        return Err("caminho não corresponde a nenhuma sessão descoberta".to_string());
    }
    sessions
        .iter()
        .find(|s| s.path.canonicalize().map(|p| p == canon).unwrap_or(false))
        .ok_or_else(|| "caminho não corresponde a nenhuma sessão descoberta".to_string())
}

// The JSONL stub/rewrite engine (content_pointers, stub_text,
// build_stub_replacements, stub_replacement_line) lives in [`crate::stub`] —
// pure and line-targeting-consistent with `ng_sessions::rewrite` (finding
// 19b); see that module's doc.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_index_html_embeds_token_and_removes_placeholder() {
        let html = render_index_html("deadbeef1234");
        assert!(html.contains("const NG_TOKEN = \"deadbeef1234\";"));
        assert!(!html.contains("NG_TOKEN_PLACEHOLDER"));
    }

    #[test]
    fn known_path_is_allowed() {
        let allowed = vec![PathBuf::from("/a/b.jsonl"), PathBuf::from("/a/c.jsonl")];
        assert!(path_is_allowed(Path::new("/a/b.jsonl"), &allowed));
    }

    #[test]
    fn path_outside_known_list_is_refused() {
        let allowed = vec![PathBuf::from("/a/b.jsonl")];
        assert!(!path_is_allowed(Path::new("/etc/passwd"), &allowed));
        assert!(!path_is_allowed(Path::new("/a/other.jsonl"), &allowed));
    }

    #[test]
    fn resolve_session_rejects_path_not_in_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let known = tmp.path().join("known.jsonl");
        std::fs::write(&known, "{}\n").unwrap();
        let unknown = tmp.path().join("unknown.jsonl");
        std::fs::write(&unknown, "{}\n").unwrap();

        let sessions = vec![SessionInfo {
            id: "s1".into(),
            harness: "claude-code".into(),
            path: known.clone(),
            project: None,
            modified_at: std::time::SystemTime::now(),
            items_hint: None,
        }];

        assert!(resolve_session(known.to_str().unwrap(), &sessions).is_ok());
        assert!(resolve_session(unknown.to_str().unwrap(), &sessions).is_err());
    }

    fn entity(id: i64, name: &str, kind: &str, weight: f64) -> Entity {
        Entity {
            id,
            name: name.to_string(),
            kind: kind.to_string(),
            project: "/tmp/proj".to_string(),
            weight,
            updated_at: 0,
        }
    }

    #[test]
    fn build_graph_response_maps_ids_to_names() {
        let entities = vec![
            entity(1, "a", "concept", 1.0),
            entity(2, "src/x.rs", "file", 2.0),
        ];
        let edges = vec![(1, 2, 0.42)];
        let resp = build_graph_response(&entities, &edges);
        assert_eq!(
            resp,
            GraphResponse {
                nodes: vec![
                    GraphNode {
                        name: "a".into(),
                        kind: "concept".into(),
                        weight: 1.0
                    },
                    GraphNode {
                        name: "src/x.rs".into(),
                        kind: "file".into(),
                        weight: 2.0
                    },
                ],
                edges: vec![GraphEdge {
                    a: "a".into(),
                    b: "src/x.rs".into(),
                    weight: 0.42
                }],
            }
        );
    }

    #[test]
    fn build_graph_response_drops_edges_with_unknown_endpoint() {
        let entities = vec![entity(1, "a", "concept", 1.0)];
        let edges = vec![(1, 999, 0.5)];
        let resp = build_graph_response(&entities, &edges);
        assert!(
            resp.edges.is_empty(),
            "aresta apontando para entidade fora do conjunto deve ser descartada"
        );
    }

    #[test]
    fn build_graph_response_empty_input_is_empty_output() {
        let resp = build_graph_response(&[], &[]);
        assert!(resp.nodes.is_empty());
        assert!(resp.edges.is_empty());
    }

    #[test]
    fn validate_bump_rejects_empty_or_blank_name() {
        assert!(validate_bump(&BumpRequest {
            name: "".into(),
            delta: 1.0
        })
        .is_err());
        assert!(validate_bump(&BumpRequest {
            name: "   ".into(),
            delta: 1.0
        })
        .is_err());
    }

    #[test]
    fn validate_bump_rejects_non_finite_delta() {
        assert!(validate_bump(&BumpRequest {
            name: "a".into(),
            delta: f64::NAN
        })
        .is_err());
        assert!(validate_bump(&BumpRequest {
            name: "a".into(),
            delta: f64::INFINITY
        })
        .is_err());
        assert!(validate_bump(&BumpRequest {
            name: "a".into(),
            delta: f64::NEG_INFINITY
        })
        .is_err());
    }

    #[test]
    fn validate_bump_rejects_out_of_range_delta() {
        assert!(validate_bump(&BumpRequest {
            name: "a".into(),
            delta: GRAPH_BUMP_MAX + 0.1
        })
        .is_err());
        assert!(validate_bump(&BumpRequest {
            name: "a".into(),
            delta: -(GRAPH_BUMP_MAX + 0.1)
        })
        .is_err());
    }

    #[test]
    fn validate_bump_accepts_reasonable_request() {
        assert!(validate_bump(&BumpRequest {
            name: "topic".into(),
            delta: 0.5
        })
        .is_ok());
        assert!(validate_bump(&BumpRequest {
            name: "topic".into(),
            delta: -GRAPH_BUMP_MAX
        })
        .is_ok());
        assert!(validate_bump(&BumpRequest {
            name: "topic".into(),
            delta: GRAPH_BUMP_MAX
        })
        .is_ok());
    }

    #[test]
    fn memory_scope_global_by_default() {
        assert_eq!(resolve_memory_scope(None, Some("/p")), None);
        assert_eq!(resolve_memory_scope(Some("global"), Some("/p")), None);
    }

    #[test]
    fn memory_scope_project_needs_nonblank_name() {
        assert_eq!(
            resolve_memory_scope(Some("project"), Some("/p")),
            Some("/p")
        );
        assert_eq!(resolve_memory_scope(Some("project"), Some("   ")), None);
        assert_eq!(resolve_memory_scope(Some("project"), None), None);
    }

    #[test]
    fn validate_manual_memory_rejects_blank_and_trims() {
        assert!(validate_manual_memory("   ").is_err());
        assert!(validate_manual_memory("").is_err());
        assert_eq!(validate_manual_memory("  hi  ").unwrap(), "hi");
    }

    #[test]
    fn validate_manual_memory_rejects_oversize() {
        let big = "a".repeat(MEMORY_MAX_CONTENT + 1);
        assert!(validate_manual_memory(&big).is_err());
    }
}
