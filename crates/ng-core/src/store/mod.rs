//! SQLite store: WAL journal, FTS5 index, hybrid-ready schema.

mod assist;
mod embeddings;
mod gain;
mod graph;
mod memories;
mod saver;
mod search;
mod stats;
mod util;

#[cfg(test)]
mod tests;

use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};

use crate::Result;

pub use assist::{PendingImport, PendingScan};

/// A search result with provenance — provenance is what lets the model cite
/// instead of hallucinate, so it is never optional.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub id: i64,
    pub session_id: String,
    pub project: String,
    pub harness: String,
    pub kind: String,
    pub snippet: String,
    pub tags: String,
    pub created_at: i64,
    pub rank: f64,
}

/// One stored memory as surfaced by the "Memória" view: a captured (or
/// manually added) event enriched with its soft-state (hidden/annotated)
/// flags. Distinct from [`SearchHit`] — this carries the full content and
/// the user-editable soft columns, not a search snippet/rank.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Memory {
    pub id: i64,
    pub project: String,
    pub harness: String,
    pub kind: String,
    pub content: String,
    pub tags: String,
    pub tokens_est: i64,
    pub created_at: i64,
    /// `true` when the row has a non-null `hidden_at` — excluded from
    /// recall/injection but never deleted, restorable via `unhide_memory`.
    pub hidden: bool,
    /// User annotation, if any.
    pub note: Option<String>,
    /// `true` for memories the user added by hand (not captured from a
    /// harness session).
    pub manual: bool,
}

/// A node in the wisdom graph: a file, concept, error, or decision the
/// captured events talk about.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entity {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub project: String,
    pub weight: f64,
    pub updated_at: i64,
}

/// Weight cap shared by entities and relations — an unbounded reinforcement
/// loop would let one hot topic drown out everything else in `neighbors`
/// and `export_graph_md` rankings.
const MAX_GRAPH_WEIGHT: f64 = 10.0;
/// Per-reappearance reinforcement for both entities and cooccurrence edges.
const GRAPH_REINFORCE_STEP: f64 = 0.1;
/// Decisions start hotter than other entity kinds: a recorded decision is
/// immediately more load-bearing than an incidentally-mentioned file.
const DECISION_INITIAL_WEIGHT: f64 = 3.0;
const DEFAULT_INITIAL_WEIGHT: f64 = 1.0;

/// `ngd` opens several read-write connections to a possibly brand-new
/// database within milliseconds of each other at startup (writer thread,
/// enrichment worker); the `journal_mode = WAL` conversion on a not-yet-
/// existing file needs a lock that a sibling connection mid-creation can
/// hold, and that collision surfaces as an immediate "database is locked"
/// even with `busy_timeout` set — it's not the ordinary statement-level
/// contention `busy_timeout` covers. [`Store::open`] retries the whole
/// open+init sequence to ride out that one-time startup race.
const OPEN_RETRY_ATTEMPTS: u32 = 10;
const OPEN_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

/// O fallback direto do `ng-hook` (daemon fora do ar) roda no hot path de
/// <5ms: os 10 × 300ms de [`OPEN_RETRY_ATTEMPTS`] — calibrados para a corrida
/// única de criação do banco no cold start do daemon — significariam até ~3s
/// bloqueando a sessão do usuário a cada prompt. [`Store::open_bounded`] usa
/// este orçamento curto em vez do global: perder uma captura é aceitável,
/// segurar o prompt não é.
const BOUNDED_OPEN_RETRY_ATTEMPTS: u32 = 3;
const BOUNDED_OPEN_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

pub struct Store {
    conn: Connection,
    /// `temp.term_probe`/`term_probe_vocab` já criadas nesta conexão?
    /// (tabelas temp são por-conexão; ver `tokenize_like_index` em
    /// `store/search.rs`.) `Cell` porque os métodos de busca usam `&self`.
    probe_ready: std::cell::Cell<bool>,
}

impl Store {
    /// Open (creating if needed) the database at `path`, retrying past the
    /// transient startup lock race documented at [`OPEN_RETRY_ATTEMPTS`].
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_retry(path, OPEN_RETRY_ATTEMPTS, OPEN_RETRY_INTERVAL)
    }

    /// Variante de tempo limitado do [`Store::open`] para caminhos quentes
    /// (o fallback direto do `ng-hook`): mesmas semânticas de criação/init,
    /// mas com o orçamento curto de [`BOUNDED_OPEN_RETRY_ATTEMPTS`] em vez
    /// dos ~3s do open de cold start — ver o comentário nas constantes.
    pub fn open_bounded(path: &Path) -> Result<Self> {
        Self::open_with_retry(
            path,
            BOUNDED_OPEN_RETRY_ATTEMPTS,
            BOUNDED_OPEN_RETRY_INTERVAL,
        )
    }

    fn open_with_retry(path: &Path, attempts: u32, interval: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut last_err = None;
        for attempt in 0..attempts {
            match Connection::open(path)
                .map_err(Into::into)
                .and_then(Self::init)
            {
                Ok(store) => return Ok(store),
                Err(err) => {
                    if attempt + 1 < attempts {
                        std::thread::sleep(interval);
                    }
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.expect("loop runs at least once"))
    }

    /// Open an existing database read-only (used by `ng search`/`ng status`
    /// so the CLI never contends with the daemon's writer).
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self {
            conn,
            probe_ready: std::cell::Cell::new(false),
        })
    }

    /// Abre RW um banco cujo schema JÁ foi garantido por outro processo/
    /// conexão (o `ngd` roda [`Store::open`] — e portanto `init` — no boot):
    /// aplica só os pragmas de runtime por conexão, sem re-executar o DDL
    /// idempotente nem o retry de cold start de [`OPEN_RETRY_ATTEMPTS`].
    /// Para conexões curtas de escrita da UI. Recusa um banco inexistente em
    /// vez de criá-lo: `Connection::open` criaria um arquivo vazio SEM
    /// schema, e toda escrita seguinte falharia de forma confusa.
    pub fn open_rw_no_init(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(crate::Error::Other(
                "banco ainda não existe — o daemon cria no boot".to_string(),
            ));
        }
        let conn = Connection::open(path)?;
        // Mesmos pragmas de runtime que `init` aplica por conexão; nada de
        // CREATE/ALTER (o schema já está garantido).
        conn.pragma_update(None, "busy_timeout", 5000_i64)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self {
            conn,
            probe_ready: std::cell::Cell::new(false),
        })
    }

    fn init(conn: Connection) -> Result<Self> {
        // busy_timeout must be set BEFORE journal_mode: the daemon now opens
        // multiple connections at near-simultaneous startup (writer thread +
        // enrichment worker), and the journal_mode pragma itself can block on
        // another connection's WAL setup — without a timeout in effect yet,
        // that collision surfaces as an immediate "database is locked" error
        // instead of a bounded retry.
        conn.pragma_update(None, "busy_timeout", 5000_i64)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                id          INTEGER PRIMARY KEY,
                session_id  TEXT NOT NULL,
                project     TEXT NOT NULL,
                harness     TEXT NOT NULL,
                kind        TEXT NOT NULL,
                content     TEXT NOT NULL,
                tags        TEXT NOT NULL DEFAULT '',
                tokens_est  INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, id);
            CREATE INDEX IF NOT EXISTS idx_events_project ON events(project, created_at);

            CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
                content, tags,
                content='events', content_rowid='id',
                tokenize='unicode61 remove_diacritics 2'
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS events_fts_vocab
                USING fts5vocab('events_fts', 'row');
            CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events BEGIN
                INSERT INTO events_fts(rowid, content, tags)
                VALUES (new.id, new.content, new.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS events_au AFTER UPDATE ON events BEGIN
                INSERT INTO events_fts(events_fts, rowid, content, tags)
                VALUES ('delete', old.id, old.content, old.tags);
                INSERT INTO events_fts(rowid, content, tags)
                VALUES (new.id, new.content, new.tags);
            END;
            -- events_fts is external-content (content='events'): the index
            -- doesn't own its rows, so a DELETE on events leaves that row's
            -- postings behind unless something tells fts5 to drop them.
            -- Nothing in this codebase deletes events today, but future
            -- retention/pruning will — this is the standard fts5
            -- external-content DELETE trigger, added now so the index
            -- can never silently accumulate orphaned postings later.
            CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events BEGIN
                INSERT INTO events_fts(events_fts, rowid, content, tags)
                VALUES ('delete', old.id, old.content, old.tags);
            END;

            CREATE TABLE IF NOT EXISTS embeddings (
                event_id    INTEGER PRIMARY KEY REFERENCES events(id),
                model       TEXT NOT NULL,
                vec         BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS entities (
                id          INTEGER PRIMARY KEY,
                name        TEXT NOT NULL,
                kind        TEXT NOT NULL,
                project     TEXT NOT NULL DEFAULT '',
                weight      REAL NOT NULL DEFAULT 1.0,
                updated_at  INTEGER NOT NULL,
                UNIQUE(name, kind, project)
            );
            CREATE TABLE IF NOT EXISTS relations (
                a           INTEGER NOT NULL REFERENCES entities(id),
                b           INTEGER NOT NULL REFERENCES entities(id),
                kind        TEXT NOT NULL,
                weight      REAL NOT NULL DEFAULT 1.0,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (a, b, kind)
            );
            -- Speeds the `kind = 'cooccurs'` filter in load_adjacency (which
            -- also restricts a/b via IN (...) built from the id set already
            -- in hand, rather than materializing the whole table in Rust).
            CREATE INDEX IF NOT EXISTS idx_relations_kind ON relations(kind);
            -- Speeds load_scoped_entities' `project = ?` filter.
            CREATE INDEX IF NOT EXISTS idx_entities_project ON entities(project);

            -- Single-row cursor (enforced by the CHECK) remembering the last
            -- events.id folded into the graph, so re-ingestion only walks
            -- new events instead of rescanning the whole table each pass.
            CREATE TABLE IF NOT EXISTS graph_cursor (
                id          INTEGER PRIMARY KEY CHECK (id = 1),
                last_event  INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO graph_cursor (id, last_event) VALUES (1, 0);

            -- Metadados do grafo derivado (versão de regras de extração).
            CREATE TABLE IF NOT EXISTS graph_meta (
                key    TEXT PRIMARY KEY,
                value  TEXT NOT NULL
            );

            -- Cursor da importação de assistant (session_end processados):
            -- mesmo padrão single-row do graph_cursor.
            CREATE TABLE IF NOT EXISTS assist_cursor (
                id          INTEGER PRIMARY KEY CHECK (id = 1),
                last_event  INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO assist_cursor (id, last_event) VALUES (1, 0);

            -- Quantos itens assistant de cada transcript já viraram eventos —
            -- Stop dispara N vezes por sessão; sem isto cada Stop re-importaria
            -- a sessão inteira.
            CREATE TABLE IF NOT EXISTS transcript_cursor (
                session_id      TEXT PRIMARY KEY,
                imported_items  INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            );

            -- Métrica operacional (não memória): uma linha por injeção
            -- servida ou passada de higiene aplicada. `tokens` é custo
            -- injetado (kind='inject') ou economia líquida de higiene
            -- (kind IN ('evict','clear')). Nunca entra em FTS/busca/injeção;
            -- aditiva por construção (IF NOT EXISTS, nada existente muda).
            CREATE TABLE IF NOT EXISTS gain_ledger (
                id          INTEGER PRIMARY KEY,
                kind        TEXT NOT NULL,
                session_id  TEXT NOT NULL,
                project     TEXT NOT NULL,
                tokens      INTEGER NOT NULL,
                items       INTEGER NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_gain_ledger_project
                ON gain_ledger(project, created_at);

            -- Estado do gate de medição dos savers externos (plano 004):
            -- disabled → measured (bench rodou) → trusted (único estado em
            -- que o digest entra em stub) → demoted (ficou líquido-negativo).
            -- Aditiva por construção; ausência de linha = nunca medido.
            CREATE TABLE IF NOT EXISTS saver_state (
                name        TEXT PRIMARY KEY,
                status      TEXT NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            "#,
        )?;

        // Additive soft-state columns for own-memory editing (Memória view).
        // Kept out of the CREATE TABLE above and applied here so both a
        // brand-new database and an existing one converge to the same schema
        // through one guarded path — the invariant is that nothing captured is
        // ever DELETEd, so hiding a memory only sets `hidden_at`, which the
        // search/injection paths treat as "not recallable" while the row (and
        // its FTS/embedding) stay in the DB, recoverable by clearing it again.
        ensure_column(&conn, "events", "hidden_at", "INTEGER")?;
        ensure_column(&conn, "events", "note", "TEXT")?;
        ensure_column(&conn, "events", "manual", "INTEGER NOT NULL DEFAULT 0")?;

        // Colunas derivadas dos savers externos (plano 004): o worker do
        // `ngd` grava um digest pré-computado aqui; `content` NUNCA é
        // tocado — o original fica no banco sempre, o digest é uma
        // projeção aditiva que o builder de stub pode preferir. `saved_by`
        // preenchido com digest NULL = saver tentou e falhou (pass-through,
        // não re-tentado a cada poll).
        ensure_column(&conn, "events", "saved_digest", "TEXT")?;
        ensure_column(&conn, "events", "saved_ref", "TEXT")?;
        ensure_column(&conn, "events", "saved_by", "TEXT")?;
        // Meta estruturada de captura (fase grafo-saneado): JSON com
        // tool/file_path/transcript_path. Aditiva; NULL para o histórico.
        ensure_column(&conn, "events", "meta", "TEXT")?;
        // Seam do ledger (plano 004 §4): kinds futuros `saver_evict`/
        // `saver_retrieve` gravarão qual saver gerou a linha. A coluna
        // existe já para o schema convergir; os kinds ainda não são
        // emitidos nesta fase (consumo do digest pela higiene é etapa 6).
        ensure_column(&conn, "gain_ledger", "saver", "TEXT")?;

        // Regras de extração do grafo mudaram? entities/relations são
        // derivadas puras de events — wipe + cursor 0 e o worker (ou
        // `ng wisdom --rebuild`) re-ingere tudo com as regras novas.
        graph::ensure_rules_version(&conn)?;

        Ok(Self {
            conn,
            probe_ready: std::cell::Cell::new(false),
        })
    }
}

/// Idempotently add `column` (`decl` = its type/constraints) to `table` if it
/// isn't already present. `table`/`column`/`decl` are always internal
/// constants (never request data), so formatting them into the DDL is safe —
/// there is no SQL-injection surface here. Used for additive schema migrations
/// that must converge a fresh and an existing database to the same shape.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        params![table, column],
        |r| r.get(0),
    )?;
    if present == 0 {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}
