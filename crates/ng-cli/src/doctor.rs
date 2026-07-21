//! `ng doctor`: environment diagnostics, one line per check, each ✗/! with a
//! one-line fix. Exits 1 if any check is a hard failure (✗); warnings (!)
//! never fail the process — they're advisory.

use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use ng_core::{paths, Embedder, HashEmbedder, Store};
use serde_json::Value;

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;
use crate::util::find_sibling_binary;

/// Must match `install::HOOK_EVENTS` — kept separate rather than shared
/// because doctor additionally checks for `PRECOMPACT_EVENT`, which install
/// does not yet wire up (that's exactly the gap doctor flags).
const HOOK_EVENTS: &[&str] = &["UserPromptSubmit", "PostToolUse", "SessionStart", "Stop"];
const PRECOMPACT_EVENT: &str = "PreCompact";
/// Must match `ngd::ui`'s `NG_UI_PORT` fallback.
const DEFAULT_UI_PORT: u16 = 4949;
const EMBED_BACKLOG_QUERY_LIMIT: usize = 2000;
const EMBED_BACKLOG_WARN_AT: usize = 500;
/// Acima disso o doctor sugere higiene — limiar arbitrário-razoável; nada é
/// deletado do banco, o aviso só aponta o `ng clear`.
const DB_SIZE_WARN_BYTES: u64 = 512 * 1024 * 1024;
/// As sondas de integridade precisam de conexão RW (ver os comentários em
/// [`read_quick_check`] e [`read_fts_integrity`]) — timeout curto para nunca
/// ficarem presas atrás do writer do daemon.
const INTEGRITY_PROBE_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

struct Check {
    status: Status,
    line: String,
}

impl Check {
    fn ok(line: String) -> Self {
        Check {
            status: Status::Ok,
            line,
        }
    }
    fn warn(line: String) -> Self {
        Check {
            status: Status::Warn,
            line,
        }
    }
    fn fail(line: String) -> Self {
        Check {
            status: Status::Fail,
            line,
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let m = Msgs::get();
    // A ordem dos grupos é a ordem antiga do Vec achatado — só ganhou
    // cabeçalhos de seção entre eles (banner → seções → linhas de check).
    let groups: [(&str, Vec<Check>); 6] = [
        (m.doc_sec_binaries, check_binaries(m)),
        (m.doc_sec_daemon, check_daemon(m)),
        (m.doc_sec_database, check_database(m)),
        (m.doc_sec_hooks, check_hooks(m)),
        (m.doc_sec_ui, vec![check_ui(m)]),
        (
            m.doc_sec_embeddings,
            check_embedding_backlog(m).into_iter().collect(),
        ),
    ];

    let p = Palette::detect();
    println!("{}", p.banner(m.doc_banner, ""));

    let mut has_fail = false;
    for (title, checks) in &groups {
        if checks.is_empty() {
            continue;
        }
        println!();
        println!("{}", p.section(title));
        for c in checks {
            println!("  {}", styled_line(&p, c));
            if c.status == Status::Fail {
                has_fail = true;
            }
        }
    }

    if has_fail {
        std::process::exit(1);
    }
    Ok(())
}

/// Renderização da linha de um check: o glifo ganha a cor do status e a
/// dica de correção (tudo depois do primeiro " — ") vai apagada. O texto de
/// `Check.line` NUNCA é reescrito — os testes unitários o fixam; sem cor
/// (pipe) só entram o glifo e a indentação, nunca bytes ESC.
fn styled_line(p: &Palette, check: &Check) -> String {
    let glyph = match check.status {
        Status::Ok => p.ok_glyph(),
        Status::Warn => p.warn_glyph(),
        Status::Fail => p.err_glyph(),
    };
    match check.line.split_once(" — ") {
        Some((msg, hint)) => format!("{glyph} {msg} {}", p.dim(format!("— {hint}"))),
        None => format!("{glyph} {}", check.line),
    }
}

fn check_binaries(m: &Msgs) -> Vec<Check> {
    ["ng-hook", "ngd"]
        .iter()
        .map(|name| match find_sibling_binary(name) {
            Some(path) => Check::ok(fill(
                m.doc_bin_found,
                &[("{name}", name), ("{path}", &path.display())],
            )),
            None => Check::fail(fill(m.doc_bin_missing, &[("{name}", name)])),
        })
        .collect()
}

fn check_daemon(m: &Msgs) -> Vec<Check> {
    use std::io::Write;

    let socket = paths::socket_path();
    let start = Instant::now();
    match UnixStream::connect(&socket) {
        Ok(mut stream) => {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            // An empty line is a documented no-op in the daemon's read loop
            // (ngd's handler skips blank lines before JSON parsing), so this
            // ping never inserts a bogus event.
            let write_ok = stream.write_all(b"\n").is_ok();
            let elapsed = start.elapsed();
            if write_ok {
                vec![Check::ok(fill(
                    m.doc_daemon_ok,
                    &[
                        ("{socket}", &socket.display()),
                        ("{ms}", &format!("{:.1}", elapsed.as_secs_f64() * 1000.0)),
                    ],
                ))]
            } else {
                vec![Check::warn(fill(
                    m.doc_daemon_refused,
                    &[("{socket}", &socket.display())],
                ))]
            }
        }
        Err(_) => vec![Check::fail(fill(
            m.doc_daemon_down,
            &[("{socket}", &socket.display())],
        ))],
    }
}

fn check_database(m: &Msgs) -> Vec<Check> {
    let db = paths::db_path();
    if !db.exists() {
        return vec![Check::warn(fill(
            m.doc_db_absent,
            &[("{path}", &db.display())],
        ))];
    }

    let mut checks = Vec::new();
    let store = match Store::open_readonly(&db) {
        Ok(store) => store,
        Err(err) => {
            return vec![Check::fail(fill(
                m.doc_db_open_fail,
                &[("{path}", &db.display()), ("{err}", &err)],
            ))];
        }
    };

    let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    match store.stats() {
        Ok((events, sessions, tokens)) => {
            checks.push(Check::ok(fill(
                m.doc_db_stats,
                &[
                    ("{path}", &db.display()),
                    ("{events}", &events),
                    ("{sessions}", &sessions),
                    ("{tokens}", &tokens),
                    ("{mib}", &format!("{:.1}", size as f64 / (1024.0 * 1024.0))),
                ],
            )));
        }
        Err(err) => checks.push(Check::fail(fill(m.doc_db_stats_fail, &[("{err}", &err)]))),
    }

    checks.push(match read_journal_mode(&db) {
        Ok(mode) => journal_mode_check(&mode, m),
        Err(err) => Check::warn(fill(m.doc_journal_fail, &[("{err}", &err)])),
    });

    if let Some(check) = db_size_check(size, m) {
        checks.push(check);
    }

    checks.push(match read_quick_check(&db) {
        Ok((first_row, elapsed)) => quick_check_check(&first_row, elapsed, &db, m),
        Err(err) => Check::warn(fill(m.doc_quick_fail, &[("{err}", &err)])),
    });

    checks.push(match read_fts_integrity(&db) {
        Ok(outcome) => fts_integrity_check(outcome, m),
        Err(err) => Check::warn(fill(m.doc_fts_fail, &[("{err}", &err)])),
    });

    checks
}

/// Pura: transforma o resultado de `PRAGMA quick_check` num Check — separada
/// da leitura de disco, como [`journal_mode_check`], para ser unit-testável.
fn quick_check_check(first_row: &str, elapsed: Duration, db: &Path, m: &Msgs) -> Check {
    if first_row.eq_ignore_ascii_case("ok") {
        // quick_check é O(banco); só vale registrar o tempo quando ele
        // começa a doer (>= 1s), senão a linha vira ruído.
        if elapsed.as_secs_f64() >= 1.0 {
            Check::ok(fill(
                m.doc_quick_ok_timed,
                &[("{s}", &format!("{:.1}", elapsed.as_secs_f64()))],
            ))
        } else {
            Check::ok(m.doc_quick_ok.to_string())
        }
    } else {
        Check::fail(fill(
            m.doc_quick_fail_row,
            &[("{row}", &first_row), ("{db}", &db.display())],
        ))
    }
}

fn read_quick_check(db: &Path) -> anyhow::Result<(String, Duration)> {
    // RW a contragosto: o SQLite bundled valida o índice invertido do FTS5
    // dentro do próprio quick_check, e esse validador precisa de acesso de
    // escrita — numa conexão read-only um banco saudável reporta o falso
    // positivo "unable to validate the inverted index ... attempt to write a
    // readonly database". O quick_check em si nunca modifica dados; o
    // busy_timeout curto e o escopo de função garantem que a conexão não
    // compete com o writer do daemon.
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(INTEGRITY_PROBE_BUSY_TIMEOUT)?;
    let start = Instant::now();
    // `(1)` limita a 1 erro reportado — basta para diagnóstico.
    let first_row: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    Ok((first_row, start.elapsed()))
}

/// Pura: mapeia o desfecho do integrity-check FTS5 num Check. Banco ocupado
/// NÃO é corrupção — vira Warn, nunca Fail.
fn fts_integrity_check(outcome: Result<(), String>, m: &Msgs) -> Check {
    match outcome {
        Ok(()) => Check::ok(m.doc_fts_ok.to_string()),
        Err(err) if err.contains("database is locked") || err.contains("busy") => {
            Check::warn(m.doc_fts_busy.to_string())
        }
        Err(err) => Check::fail(fill(m.doc_fts_corrupt, &[("{err}", &err)])),
    }
}

/// Roda o integrity-check do FTS5 external-content. Exige conexão RW (é um
/// comando via INSERT na tabela virtual), mas não modifica dados; a conexão
/// abre com [`INTEGRITY_PROBE_BUSY_TIMEOUT`] e fecha no fim do escopo para nunca
/// competir com o writer do daemon.
fn read_fts_integrity(db: &Path) -> anyhow::Result<Result<(), String>> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(INTEGRITY_PROBE_BUSY_TIMEOUT)?;
    // O `rank=1` compara o índice com o conteúdo externo; divergência sai
    // como erro SQLITE_CORRUPT_VTAB.
    match conn.execute(
        "INSERT INTO events_fts(events_fts, rank) VALUES('integrity-check', 1)",
        [],
    ) {
        Ok(_) => Ok(Ok(())),
        Err(err) => Ok(Err(err.to_string())),
    }
}

/// Pura: avisa quando o banco passa de [`DB_SIZE_WARN_BYTES`].
fn db_size_check(bytes: u64, m: &Msgs) -> Option<Check> {
    if bytes < DB_SIZE_WARN_BYTES {
        return None;
    }
    Some(Check::warn(fill(
        m.doc_dbsize_warn,
        &[(
            "{gib}",
            &format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0)),
        )],
    )))
}

/// Pure: turns a `PRAGMA journal_mode` result into a Check. Split out from
/// [`read_journal_mode`] (which needs a real file) so the threshold logic is
/// unit-testable without touching disk.
fn journal_mode_check(mode: &str, m: &Msgs) -> Check {
    if mode.eq_ignore_ascii_case("wal") {
        Check::ok(fill(m.doc_journal_ok, &[("{mode}", &mode)]))
    } else {
        Check::warn(fill(m.doc_journal_warn, &[("{mode}", &mode)]))
    }
}

fn read_journal_mode(db: &Path) -> anyhow::Result<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    Ok(mode)
}

fn check_hooks(m: &Msgs) -> Vec<Check> {
    let mut checks = Vec::new();
    let project_path = std::env::current_dir()
        .ok()
        .map(|d| d.join(".claude/settings.json"));
    let global_path = dirs::home_dir().map(|h| h.join(".claude/settings.json"));

    let mut any_installed = false;
    for (label, path) in [
        (m.doc_hooks_label_project, project_path),
        (m.doc_hooks_label_global, global_path),
    ] {
        let Some(path) = path else { continue };
        if !path.exists() {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        match parsed {
            Some(settings) => {
                let covered = hooks_covered(&settings);
                if covered.is_empty() {
                    continue;
                }
                any_installed = true;
                checks.push(Check::ok(fill(
                    m.doc_hooks_covered,
                    &[
                        ("{label}", &label),
                        ("{path}", &path.display()),
                        ("{events}", &covered.join(", ")),
                    ],
                )));
                // O comando registrado carrega um path absoluto (é o que
                // `ng install` escreve); depois de mover o repo ou apagar
                // `target/`, o hook morre em silêncio — checar só a string
                // conter "ng-hook" pintaria tudo de verde mesmo assim.
                for hook_path in ng_hook_paths(&settings) {
                    if hook_path.starts_with('/') && !Path::new(&hook_path).exists() {
                        checks.push(Check::fail(fill(
                            m.doc_hooks_dangling,
                            &[("{path}", &path.display()), ("{hook}", &hook_path)],
                        )));
                    }
                }
                if !covered.iter().any(|e| e == PRECOMPACT_EVENT) {
                    checks.push(Check::warn(fill(
                        m.doc_hooks_precompact_missing,
                        &[("{event}", &PRECOMPACT_EVENT), ("{path}", &path.display())],
                    )));
                }
            }
            None => checks.push(Check::warn(fill(
                m.doc_hooks_invalid_json,
                &[("{path}", &path.display())],
            ))),
        }
    }

    if !any_installed {
        checks.push(Check::fail(m.doc_hooks_none.to_string()));
    }

    checks
}

/// Pure: given a `settings.json` `Value`, returns which of the known hook
/// events (the four `install::HOOK_EVENTS` plus `PreCompact`) have an
/// `ng-hook` command wired under them.
fn hooks_covered(settings: &Value) -> Vec<String> {
    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return Vec::new();
    };
    HOOK_EVENTS
        .iter()
        .chain(std::iter::once(&PRECOMPACT_EVENT))
        .filter(|event| event_has_ng_hook(hooks, event))
        .map(|event| event.to_string())
        .collect()
}

fn event_has_ng_hook(hooks: &serde_json::Map<String, Value>, event: &str) -> bool {
    hooks
        .get(event)
        .and_then(|v| v.as_array())
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .pointer("/hooks")
                    .and_then(|h| h.as_array())
                    .is_some_and(|inner| {
                        inner.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .is_some_and(|c| c.contains("ng-hook"))
                        })
                    })
            })
        })
}

/// Pure: coleta os paths de binário de todo comando `ng-hook` registrado em
/// qualquer evento de `settings.json`, deduplicados e na ordem de aparição.
fn ng_hook_paths(settings: &Value) -> Vec<String> {
    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return Vec::new();
    };
    let mut paths: Vec<String> = Vec::new();
    for entries in hooks.values().filter_map(|v| v.as_array()) {
        for inner in entries
            .iter()
            .filter_map(|entry| entry.pointer("/hooks").and_then(|h| h.as_array()))
        {
            for command in inner
                .iter()
                .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
                .filter(|c| c.contains("ng-hook"))
            {
                if let Some(hook_path) = extract_hook_path(command) {
                    if !paths.contains(&hook_path) {
                        paths.push(hook_path);
                    }
                }
            }
        }
    }
    paths
}

/// Pure: extrai o token de path de um comando ng-hook. Cobre as duas formas
/// que `ng install` escreve: o path pelado do Claude Code e a shell-quoted
/// de Kimi/Gemini (`env NG_HARNESS=... '<path>'`, com o escape `'\''` de
/// `install::shell_quote` desfeito).
fn extract_hook_path(command: &str) -> Option<String> {
    let command = command.trim();
    if let (Some(start), Some(end)) = (command.find('\''), command.rfind('\'')) {
        if end > start {
            return Some(command[start + 1..end].replace(r"'\''", "'"));
        }
    }
    // Sem citação: pula um eventual prefixo `env VAR=...` e o resto é o
    // próprio path (Claude Code registra o binário sem prefixo nenhum).
    let mut rest = command;
    if let Some(stripped) = rest.strip_prefix("env ") {
        rest = stripped.trim_start();
        while let Some((token, tail)) = rest.split_once(' ') {
            if token.contains('=') {
                rest = tail.trim_start();
            } else {
                break;
            }
        }
    }
    let rest = rest.trim();
    if rest.contains("ng-hook") {
        Some(rest.to_string())
    } else {
        None
    }
}

fn check_ui(m: &Msgs) -> Check {
    let port: u16 = std::env::var("NG_UI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_UI_PORT);
    let addr = format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback address always parses");
    match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
        Ok(_) => Check::ok(fill(m.doc_ui_ok, &[("{port}", &port)])),
        Err(_) => Check::warn(fill(m.doc_ui_warn, &[("{port}", &port)])),
    }
}

fn check_embedding_backlog(m: &Msgs) -> Option<Check> {
    let db = paths::db_path();
    if !db.exists() {
        return None;
    }
    let store = Store::open_readonly(&db).ok()?;
    let backlog = store
        .events_without_embedding(HashEmbedder.id(), EMBED_BACKLOG_QUERY_LIMIT)
        .ok()?;
    Some(backlog_check(
        backlog.len(),
        EMBED_BACKLOG_QUERY_LIMIT,
        EMBED_BACKLOG_WARN_AT,
        m,
    ))
}

/// Pure: turns a backlog count into a Check. `query_limit` caps how many
/// rows [`ng_core::Store::events_without_embedding`] was asked for, so a
/// count that hits the cap is reported as `"<limit>+"` rather than an exact
/// (possibly wrong) number.
fn backlog_check(count: usize, query_limit: usize, warn_at: usize, m: &Msgs) -> Check {
    let label = if count >= query_limit {
        format!("{query_limit}+")
    } else {
        count.to_string()
    };
    if count >= warn_at {
        Check::warn(fill(m.doc_embed_warn, &[("{count}", &label)]))
    } else {
        Check::ok(fill(m.doc_embed_ok, &[("{count}", &label)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use serde_json::json;

    /// Catálogo pt fixo: os asserts destes testes checam substrings em
    /// português, então o idioma é fixado sem depender do ambiente.
    fn pt() -> &'static Msgs {
        Msgs::for_lang(Lang::Pt)
    }

    #[test]
    fn hooks_covered_finds_ng_hook_entries() {
        let settings = json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "hooks": [{ "type": "command", "command": "/usr/local/bin/ng-hook" }] }
                ],
                "PostToolUse": [
                    { "hooks": [{ "type": "command", "command": "/usr/local/bin/some-other-hook" }] }
                ]
            }
        });
        let covered = hooks_covered(&settings);
        assert_eq!(covered, vec!["UserPromptSubmit".to_string()]);
    }

    #[test]
    fn hooks_covered_empty_without_hooks_key() {
        let settings = json!({});
        assert!(hooks_covered(&settings).is_empty());
    }

    #[test]
    fn hooks_covered_includes_precompact_when_wired() {
        let settings = json!({
            "hooks": {
                "PreCompact": [
                    { "hooks": [{ "type": "command", "command": "ng-hook" }] }
                ]
            }
        });
        assert_eq!(hooks_covered(&settings), vec!["PreCompact".to_string()]);
    }

    #[test]
    fn extract_hook_path_plain_claude_form() {
        assert_eq!(
            extract_hook_path("/home/user/repo/target/release/ng-hook"),
            Some("/home/user/repo/target/release/ng-hook".to_string())
        );
    }

    #[test]
    fn extract_hook_path_env_prefixed_quoted_form() {
        assert_eq!(
            extract_hook_path("env NG_HARNESS=kimi '/home/user/repo/target/release/ng-hook'"),
            Some("/home/user/repo/target/release/ng-hook".to_string())
        );
    }

    #[test]
    fn extract_hook_path_undoes_shell_quote_escaping() {
        // install::shell_quote escapa apóstrofos como '\'' — o caminho
        // reconstruído tem que voltar ao byte original.
        assert_eq!(
            extract_hook_path(r"env NG_HARNESS=kimi '/home/user/don'\''t/ng-hook'"),
            Some("/home/user/don't/ng-hook".to_string())
        );
    }

    #[test]
    fn extract_hook_path_env_prefixed_unquoted_form() {
        assert_eq!(
            extract_hook_path("env NG_HARNESS=gemini /opt/ng/ng-hook"),
            Some("/opt/ng/ng-hook".to_string())
        );
    }

    #[test]
    fn ng_hook_paths_collects_and_dedupes() {
        let settings = json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "hooks": [{ "type": "command", "command": "/opt/ng/ng-hook" }] }
                ],
                "PostToolUse": [
                    { "hooks": [
                        { "type": "command", "command": "/opt/ng/ng-hook" },
                        { "type": "command", "command": "/usr/bin/other-hook" }
                    ] }
                ]
            }
        });
        assert_eq!(
            ng_hook_paths(&settings),
            vec!["/opt/ng/ng-hook".to_string()]
        );
    }

    #[test]
    fn backlog_below_threshold_is_ok() {
        let check = backlog_check(10, 2000, 500, pt());
        assert_eq!(check.status, Status::Ok);
        assert!(check.line.starts_with("10 "));
    }

    #[test]
    fn backlog_at_threshold_warns() {
        let check = backlog_check(500, 2000, 500, pt());
        assert_eq!(check.status, Status::Warn);
    }

    #[test]
    fn backlog_at_query_limit_shows_plus_suffix() {
        let check = backlog_check(2000, 2000, 500, pt());
        assert_eq!(check.status, Status::Warn);
        assert!(check.line.starts_with("2000+ "), "line was: {}", check.line);
    }

    #[test]
    fn journal_mode_wal_is_ok() {
        assert_eq!(journal_mode_check("wal", pt()).status, Status::Ok);
        assert_eq!(journal_mode_check("WAL", pt()).status, Status::Ok);
    }

    #[test]
    fn journal_mode_delete_warns() {
        let check = journal_mode_check("delete", pt());
        assert_eq!(check.status, Status::Warn);
        assert!(check.line.contains("delete"));
    }

    #[test]
    fn quick_check_ok_is_ok() {
        let db = Path::new("/tmp/ng.db");
        let fast = quick_check_check("ok", Duration::from_millis(10), db, pt());
        assert_eq!(fast.status, Status::Ok);
        assert!(!fast.line.contains('s'), "sem tempo abaixo de 1s");
        // Case-insensitive e com registro do tempo quando lento.
        let slow = quick_check_check("OK", Duration::from_secs(3), db, pt());
        assert_eq!(slow.status, Status::Ok);
        assert!(slow.line.contains("3.0s"), "line was: {}", slow.line);
    }

    #[test]
    fn quick_check_error_is_fail() {
        let check = quick_check_check(
            "*** in database main ***\nPage 5 is never used",
            Duration::from_millis(10),
            Path::new("/tmp/ng.db"),
            pt(),
        );
        assert_eq!(check.status, Status::Fail);
        assert!(check.line.contains("Page 5"));
        assert!(check.line.contains("/tmp/ng.db"));
    }

    #[test]
    fn fts_busy_is_warn_not_fail() {
        let check = fts_integrity_check(Err("database is locked".to_string()), pt());
        assert_eq!(check.status, Status::Warn);
        assert!(check.line.contains("ocupado"));
    }

    #[test]
    fn fts_error_is_fail() {
        let check = fts_integrity_check(
            Err("database disk image is malformed in fts5: SQLITE_CORRUPT_VTAB".to_string()),
            pt(),
        );
        assert_eq!(check.status, Status::Fail);
        assert!(check.line.contains("SQLITE_CORRUPT_VTAB"));
    }

    #[test]
    fn fts_ok_is_ok() {
        assert_eq!(fts_integrity_check(Ok(()), pt()).status, Status::Ok);
    }

    #[test]
    fn db_size_under_threshold_is_none() {
        assert!(db_size_check(DB_SIZE_WARN_BYTES - 1, pt()).is_none());
    }

    #[test]
    fn db_size_over_threshold_warns() {
        let check =
            db_size_check(2 * 1024 * 1024 * 1024, pt()).expect("acima do limiar deve avisar");
        assert_eq!(check.status, Status::Warn);
        assert!(check.line.contains("2.0 GiB"), "line was: {}", check.line);
    }

    #[test]
    fn integrity_probes_pass_on_healthy_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ng.db");
        {
            let store = Store::open(&db).unwrap();
            store
                .insert_event(&ng_core::Event {
                    session_id: "s1".into(),
                    project: "/tmp/proj".into(),
                    harness: "claude-code".into(),
                    kind: "prompt".into(),
                    content: "um evento de teste".into(),
                    tags: "".into(),
                    meta: None,
                    created_at: 1_700_000_000,
                })
                .unwrap();
        }
        let (first_row, _elapsed) = read_quick_check(&db).unwrap();
        assert_eq!(
            quick_check_check(&first_row, Duration::ZERO, &db, pt()).status,
            Status::Ok,
            "first_row was: {first_row:?}"
        );
        let outcome = read_fts_integrity(&db).unwrap();
        assert_eq!(
            fts_integrity_check(outcome.clone(), pt()).status,
            Status::Ok,
            "outcome was: {outcome:?}"
        );
    }
}
