//! i18n: catálogo estático tipado de mensagens do CLI, sem nenhuma
//! dependência nova (nada de fluent/gettext) — só uma tabela `&'static str`
//! por idioma.
//!
//! Dois idiomas: inglês (default internacional) e português (pt-BR). O idioma
//! resolve uma vez por execução (`Msgs::get`, via `OnceLock`) a partir do
//! ambiente:
//!
//! 1. `NG_LANG` (case-insensitive: `en*`→En, `pt*`→Pt) tem precedência;
//! 2. senão o locale POSIX efetivo — o primeiro de `LC_ALL`, `LC_MESSAGES`,
//!    `LANG` que estiver setado — começando com `pt` → Pt, qualquer outro → En;
//! 3. senão En.
//!
//! Paridade garantida pelo compilador: `Msgs` tem só campos `&'static str`, e
//! as duas instâncias (`EN`/`PT`) são `static`s literais — acrescentar um
//! campo sem preencher os dois idiomas é erro de compilação ("missing field").
//!
//! Textos INTOCÁVEIS (saídas `--json`/`--md`, prefixo `^#` dos hits, exit
//! codes, strings de banco, nomes de flags/comandos do clap) NÃO passam por
//! aqui — continuam byte a byte no código como antes.

use std::fmt::Display;
use std::sync::OnceLock;

/// Idioma resolvido para a saída user-facing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Pt,
}

/// Catálogo de mensagens de um idioma. Um campo por string user-facing (ou
/// por template com placeholders `{nome}`, preenchidos por [`fill`]).
pub struct Msgs {
    // ---- compartilhado -------------------------------------------------
    pub db_missing: &'static str,
    pub ngd_not_found: &'static str,

    // ---- status --------------------------------------------------------
    pub status_banner: &'static str,
    pub status_data: &'static str,
    pub status_db: &'static str,
    pub status_db_fmt: &'static str,
    pub status_db_absent: &'static str,
    pub status_daemon: &'static str,
    pub status_daemon_running: &'static str,
    pub status_daemon_stopped: &'static str,

    // ---- search --------------------------------------------------------
    pub search_banner: &'static str,
    pub search_no_results: &'static str,
    pub search_session: &'static str,

    // ---- gain ----------------------------------------------------------
    pub gain_banner: &'static str,
    pub gain_scope_project: &'static str,
    pub gain_scope_global: &'static str,
    pub gain_counting_from: &'static str,
    pub gain_using_since: &'static str,
    pub gain_using_since_value: &'static str,
    pub gain_using_since_none: &'static str,
    pub gain_sec_memory: &'static str,
    pub gain_events_captured: &'static str,
    pub gain_sessions_tracked: &'static str,
    pub gain_tokens_stored: &'static str,
    pub gain_sec_inject: &'static str,
    pub gain_prompts_served: &'static str,
    pub gain_memories_served: &'static str,
    pub gain_tokens_injected: &'static str,
    pub gain_sec_hygiene: &'static str,
    pub gain_passes_precompact: &'static str,
    pub gain_passes_clear: &'static str,
    pub gain_items_stubbed: &'static str,
    pub gain_tokens_saved: &'static str,
    pub gain_net_ratio: &'static str,
    pub gain_no_data: &'static str,
    pub gain_footer_estimates: &'static str,
    pub gain_footer_source: &'static str,
    pub gain_since_invalid: &'static str,

    // ---- wisdom --------------------------------------------------------
    pub wisdom_json_md_exclusive: &'static str,
    pub wisdom_rebuilding: &'static str,
    pub wisdom_rebuilt: &'static str,
    pub wisdom_banner: &'static str,
    pub wisdom_scope_here: &'static str,
    pub wisdom_scope_global: &'static str,
    pub wisdom_empty: &'static str,
    pub wisdom_weight: &'static str,
    pub wisdom_no_neighbors: &'static str,
    pub wisdom_score: &'static str,

    // ---- install -------------------------------------------------------
    pub install_hook_not_found: &'static str,
    pub install_backup: &'static str,
    pub install_hooks_installed: &'static str,
    pub install_events: &'static str,
    pub install_db_init_warn: &'static str,
    pub install_hint: &'static str,

    // ---- ui ------------------------------------------------------------
    pub ui_starting_daemon: &'static str,
    pub ui_starting: &'static str,
    pub ui_daemon_failed: &'static str,

    // ---- daemon --------------------------------------------------------
    pub daemon_running: &'static str,
    pub daemon_exited: &'static str,

    // ---- memory --------------------------------------------------------
    pub mem_empty: &'static str,
    pub mem_flag_hidden: &'static str,
    pub mem_flag_manual: &'static str,
    pub mem_hidden_word: &'static str,
    pub mem_hide_ok: &'static str,
    pub mem_hide_none: &'static str,
    pub mem_unhide_word: &'static str,
    pub mem_unhide_ok: &'static str,
    pub mem_unhide_none: &'static str,
    pub mem_add_empty: &'static str,
    pub mem_add_word: &'static str,
    pub mem_add_ok: &'static str,

    // ---- sync ----------------------------------------------------------
    pub sync_seeded: &'static str,
    pub sync_synced: &'static str,
    pub sync_opencode: &'static str,

    // ---- doctor --------------------------------------------------------
    pub doc_banner: &'static str,
    pub doc_sec_binaries: &'static str,
    pub doc_sec_daemon: &'static str,
    pub doc_sec_database: &'static str,
    pub doc_sec_hooks: &'static str,
    pub doc_sec_ui: &'static str,
    pub doc_sec_embeddings: &'static str,
    pub doc_bin_found: &'static str,
    pub doc_bin_missing: &'static str,
    pub doc_daemon_ok: &'static str,
    pub doc_daemon_refused: &'static str,
    pub doc_daemon_down: &'static str,
    pub doc_db_absent: &'static str,
    pub doc_db_open_fail: &'static str,
    pub doc_db_stats: &'static str,
    pub doc_db_stats_fail: &'static str,
    pub doc_journal_fail: &'static str,
    pub doc_quick_fail: &'static str,
    pub doc_fts_fail: &'static str,
    pub doc_journal_ok: &'static str,
    pub doc_journal_warn: &'static str,
    pub doc_dbsize_warn: &'static str,
    pub doc_quick_ok_timed: &'static str,
    pub doc_quick_ok: &'static str,
    pub doc_quick_fail_row: &'static str,
    pub doc_fts_ok: &'static str,
    pub doc_fts_busy: &'static str,
    pub doc_fts_corrupt: &'static str,
    pub doc_hooks_label_project: &'static str,
    pub doc_hooks_label_global: &'static str,
    pub doc_hooks_covered: &'static str,
    pub doc_hooks_dangling: &'static str,
    pub doc_hooks_precompact_missing: &'static str,
    pub doc_hooks_invalid_json: &'static str,
    pub doc_hooks_none: &'static str,
    pub doc_ui_ok: &'static str,
    pub doc_ui_warn: &'static str,
    pub doc_embed_warn: &'static str,
    pub doc_embed_ok: &'static str,

    // ---- clap help (about + argumentos) --------------------------------
    // Localizados em runtime sobre o derive (ver `localize_help` em main.rs).
    // Um campo por texto de ajuda visível hoje: `about` da raiz, `about` de
    // cada subcomando (incl. aninhados) e `help` de cada argumento/flag.
    pub help_about: &'static str,
    pub help_cmd_install: &'static str,
    pub help_arg_install_global: &'static str,
    pub help_arg_install_harness: &'static str,
    pub help_cmd_uninstall: &'static str,
    pub help_arg_uninstall_global: &'static str,
    pub help_arg_uninstall_harness: &'static str,
    pub help_cmd_completions: &'static str,
    pub help_arg_completions_shell: &'static str,
    pub help_cmd_search: &'static str,
    pub help_arg_search_query: &'static str,
    pub help_arg_search_here: &'static str,
    pub help_arg_search_limit: &'static str,
    pub help_arg_search_semantic: &'static str,
    pub help_arg_search_json: &'static str,
    pub help_cmd_clear: &'static str,
    pub help_arg_clear_file: &'static str,
    pub help_arg_clear_target_tokens: &'static str,
    pub help_arg_clear_dry_run: &'static str,
    pub help_cmd_status: &'static str,
    pub help_arg_status_json: &'static str,
    pub help_cmd_daemon: &'static str,
    pub help_cmd_ui: &'static str,
    pub help_cmd_doctor: &'static str,
    pub help_cmd_mcp: &'static str,
    pub help_cmd_mcp_install_browser_use: &'static str,
    pub help_arg_mcp_ibu_harness: &'static str,
    pub help_arg_mcp_ibu_global: &'static str,
    pub help_cmd_sync: &'static str,
    pub help_arg_sync_personas_dir: &'static str,
    pub help_arg_sync_global: &'static str,
    pub help_cmd_dispatch: &'static str,
    pub help_arg_dispatch_prompt: &'static str,
    pub help_arg_dispatch_init: &'static str,
    pub help_cmd_wisdom: &'static str,
    pub help_arg_wisdom_here: &'static str,
    pub help_arg_wisdom_md: &'static str,
    pub help_arg_wisdom_json: &'static str,
    pub help_arg_wisdom_rebuild: &'static str,
    pub help_cmd_memory: &'static str,
    pub help_cmd_memory_list: &'static str,
    pub help_arg_memory_list_here: &'static str,
    pub help_arg_memory_list_all: &'static str,
    pub help_arg_memory_list_limit: &'static str,
    pub help_cmd_memory_hide: &'static str,
    pub help_arg_memory_hide_id: &'static str,
    pub help_cmd_memory_unhide: &'static str,
    pub help_arg_memory_unhide_id: &'static str,
    pub help_cmd_memory_add: &'static str,
    pub help_arg_memory_add_content: &'static str,
    pub help_arg_memory_add_project: &'static str,
    pub help_arg_memory_add_tags: &'static str,
    pub help_cmd_gain: &'static str,
    pub help_arg_gain_here: &'static str,
    pub help_arg_gain_json: &'static str,
    pub help_arg_gain_since: &'static str,
    pub help_cmd_saver: &'static str,
    pub help_cmd_saver_init: &'static str,
    pub help_cmd_saver_list: &'static str,
    pub help_arg_saver_list_json: &'static str,
    pub help_cmd_saver_bench: &'static str,
    pub help_arg_saver_bench_name: &'static str,
    pub help_arg_saver_bench_sample: &'static str,
    pub help_cmd_sync_context: &'static str,
    pub help_arg_sync_context_init: &'static str,
    pub help_arg_sync_context_dir: &'static str,
}

impl Msgs {
    /// Catálogo do idioma resolvido pelo ambiente, memoizado por processo.
    pub fn get() -> &'static Msgs {
        static RESOLVED: OnceLock<&'static Msgs> = OnceLock::new();
        RESOLVED.get_or_init(|| Msgs::for_lang(detect_lang()))
    }

    /// Catálogo de um idioma específico, sem consultar ambiente nem cache —
    /// o caminho que os testes usam para fixar o idioma de forma determinística.
    pub fn for_lang(lang: Lang) -> &'static Msgs {
        match lang {
            Lang::En => &EN,
            Lang::Pt => &PT,
        }
    }
}

/// Preenche placeholders `{nome}` de um template com os pares dados. Ordem
/// independente (bom para i18n: a ordem das palavras muda entre idiomas) e
/// zero-dep. Cada nome é único no template, então a substituição não colide.
pub fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = template.to_string();
    for (name, value) in args {
        out = out.replace(name, &value.to_string());
    }
    out
}

/// Resolve o idioma a partir do ambiente real (ver o doc do módulo).
pub fn detect_lang() -> Lang {
    detect_from(
        std::env::var("NG_LANG").ok().as_deref(),
        std::env::var("LC_ALL").ok().as_deref(),
        std::env::var("LC_MESSAGES").ok().as_deref(),
        std::env::var("LANG").ok().as_deref(),
    )
}

/// Núcleo puro da detecção (testável sem tocar em env): `NG_LANG` vence; senão
/// o primeiro locale POSIX setado decide (pt* → Pt, resto → En); senão En.
fn detect_from(
    ng_lang: Option<&str>,
    lc_all: Option<&str>,
    lc_messages: Option<&str>,
    lang: Option<&str>,
) -> Lang {
    if let Some(v) = ng_lang {
        let v = v.trim().to_ascii_lowercase();
        if v.starts_with("pt") {
            return Lang::Pt;
        }
        if v.starts_with("en") {
            return Lang::En;
        }
        // Valor desconhecido de NG_LANG: cai no locale, não força En.
    }
    for v in [lc_all, lc_messages, lang].into_iter().flatten() {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        if v.to_ascii_lowercase().starts_with("pt") {
            return Lang::Pt;
        }
        // Primeiro locale efetivo que não é pt → default internacional.
        return Lang::En;
    }
    Lang::En
}

static EN: Msgs = Msgs {
    // ---- compartilhado -------------------------------------------------
    db_missing: "database does not exist yet ({path}). Run `ng install` and use a session to capture memory.",
    ngd_not_found: "ngd not found (build with `cargo build --release` or put it on your PATH)",

    // ---- status --------------------------------------------------------
    status_banner: "not-goldfish · status",
    status_data: "data",
    status_db: "database",
    status_db_fmt: "{events} events · {sessions} sessions · ~{tokens} tokens · {mib} MiB",
    status_db_absent: "(not created yet)",
    status_daemon: "daemon",
    status_daemon_running: "running",
    status_daemon_stopped: "stopped (capture falls back to direct mode)",

    // ---- search --------------------------------------------------------
    search_banner: "not-goldfish · search",
    search_no_results: "no results for: {query}",
    search_session: "session",

    // ---- gain ----------------------------------------------------------
    gain_banner: "not-goldfish · accumulated gain",
    gain_scope_project: "— project {proj}",
    gain_scope_global: "— global (all projects)",
    gain_counting_from: "(counting from {date})",
    gain_using_since: "using since",
    gain_using_since_value: "{date} ({days} days)",
    gain_using_since_none: "— (no events in scope)",
    gain_sec_memory: "memory",
    gain_events_captured: "events captured",
    gain_sessions_tracked: "sessions tracked",
    gain_tokens_stored: "tokens stored",
    gain_sec_inject: "proactive injection (declared cost, not savings)",
    gain_prompts_served: "prompts served",
    gain_memories_served: "memories served",
    gain_tokens_injected: "tokens injected",
    gain_sec_hygiene: "context hygiene (real savings, counted once per eviction)",
    gain_passes_precompact: "passes (PreCompact)",
    gain_passes_clear: "passes (ng clear)",
    gain_items_stubbed: "items stubbed",
    gain_tokens_saved: "tokens saved",
    gain_net_ratio: "net ratio",
    gain_no_data: "    — (no data yet; the signal starts being recorded from this version)",
    gain_footer_estimates: "Estimates ×4 bytes/token. Injection is declared cost, not savings.",
    gain_footer_source: "Source: gain_ledger + events in the local database ({db}).",
    gain_since_invalid: "invalid --since: {s} (expected format: YYYY-MM-DD)",

    // ---- wisdom --------------------------------------------------------
    wisdom_json_md_exclusive: "--json and --md are mutually exclusive",
    wisdom_rebuilding: "rebuilding graph…",
    wisdom_rebuilt: "graph rebuilt: {n} events re-ingested",
    wisdom_banner: "not-goldfish · wisdom graph",
    wisdom_scope_here: "— current project",
    wisdom_scope_global: "— global",
    wisdom_empty: "wisdom graph still empty — capture more sessions for it to populate",
    wisdom_weight: "weight",
    wisdom_no_neighbors: "   (no neighbors)",
    wisdom_score: "score",

    // ---- install -------------------------------------------------------
    install_hook_not_found: "ng-hook not found (build with `cargo build --release` or put it on your PATH)",
    install_backup: "backup:",
    install_hooks_installed: "hooks installed in {path} ({label})",
    install_events: "events: {list}",
    install_db_init_warn: "warning: could not initialize the database now ({err}); the first capture will create it",
    install_hint: "hint: run `ng daemon` (or let the direct SQLite fallback do the work)",

    // ---- ui ------------------------------------------------------------
    ui_starting_daemon: "starting daemon…",
    ui_starting: "starting {path}",
    ui_daemon_failed: "the daemon did not start — run `ng daemon` in a terminal to see the error",

    // ---- daemon --------------------------------------------------------
    daemon_running: "running {path}",
    daemon_exited: "ngd exited with {status}",

    // ---- memory --------------------------------------------------------
    mem_empty: "no memories stored in this scope",
    mem_flag_hidden: " [hidden]",
    mem_flag_manual: " [manual]",
    mem_hidden_word: "hidden",
    mem_hide_ok: "memory {id} {word} (reversible with `ng memory unhide {raw}`)",
    mem_hide_none: "no visible memory with id {id} (maybe already hidden or nonexistent)",
    mem_unhide_word: "restored",
    mem_unhide_ok: "memory {id} {word} to search/injection",
    mem_unhide_none: "no memory with id {id}",
    mem_add_empty: "empty content — pass the memory text",
    mem_add_word: "added",
    mem_add_ok: "memory {id} {word}",

    // ---- sync ----------------------------------------------------------
    sync_seeded: "no personas in {path} — default personas (ceo, pm, dev) written there",
    sync_synced: "{count} personas synced in {path}",
    sync_opencode: "opencode.json also synced (backup: {path})",

    // ---- doctor --------------------------------------------------------
    doc_banner: "not-goldfish · doctor",
    doc_sec_binaries: "binaries",
    doc_sec_daemon: "daemon",
    doc_sec_database: "database",
    doc_sec_hooks: "hooks",
    doc_sec_ui: "web interface",
    doc_sec_embeddings: "embeddings",
    doc_bin_found: "binary {name} found at {path}",
    doc_bin_missing: "binary {name} not found — run `cargo build --release` or put {name} on your PATH",
    doc_daemon_ok: "daemon responding at {socket} ({ms}ms)",
    doc_daemon_refused: "daemon connected but refused the write at {socket} — check `ngd`'s logs",
    doc_daemon_down: "daemon not responding at {socket} — run `ng daemon` (capture still works via direct fallback)",
    doc_db_absent: "database does not exist yet at {path} — run `ng install` and use a session to capture",
    doc_db_open_fail: "could not open the database at {path}: {err}",
    doc_db_stats: "database at {path}: {events} events · {sessions} sessions · ~{tokens} tokens · {mib} MiB",
    doc_db_stats_fail: "database opened but stats() failed ({err}) — it may be corrupted, consider restoring from a backup",
    doc_journal_fail: "could not check journal_mode: {err}",
    doc_quick_fail: "could not run quick_check: {err}",
    doc_fts_fail: "could not check the FTS index: {err}",
    doc_journal_ok: "journal_mode = {mode}",
    doc_journal_warn: "journal_mode = {mode} (expected wal) — open the database once with `ng daemon` to re-enable WAL",
    doc_dbsize_warn: "database at {gib} GiB — consider `ng clear` to clean up old transcripts (nothing is deleted from the database)",
    doc_quick_ok_timed: "integrity (quick_check) ok in {s}s",
    doc_quick_ok: "integrity (quick_check) ok",
    doc_quick_fail_row: "quick_check reported: {row} — database corrupted; restore from backup or copy {db} for analysis before any write",
    doc_fts_ok: "FTS index intact (integrity-check)",
    doc_fts_busy: "couldn't check the FTS index (database busy) — stop the daemon and run again",
    doc_fts_corrupt: "FTS index out of sync/corrupted ({err}) — search and injection are unreliable; restore from a backup",
    doc_hooks_label_project: "project",
    doc_hooks_label_global: "global",
    doc_hooks_covered: "hooks ({label}) in {path}: {events}",
    doc_hooks_dangling: "the hook in {path} points to `{hook}` which does not exist — run `ng install` again",
    doc_hooks_precompact_missing: "hook {event} missing in {path} — automatic hygiene (NG_AUTO_HYGIENE) won't fire without it, run `ng install` again after updating",
    doc_hooks_invalid_json: "{path} exists but is not valid JSON — could not check hooks",
    doc_hooks_none: "no ng-hook hook installed — run `ng install` (or `ng install --global`)",
    doc_ui_ok: "UI responding at http://127.0.0.1:{port}",
    doc_ui_warn: "UI not responding at 127.0.0.1:{port} — run `ng ui` to open",
    doc_embed_warn: "{count} events without embedding — enrichment lagging, check that `ngd` is running",
    doc_embed_ok: "{count} events awaiting embedding (within normal)",

    // ---- clap help (about + argumentos) --------------------------------
    help_about: "not-goldfish: universal memory for AI harnesses",
    help_cmd_install: "Register the not-goldfish hooks in a harness (Claude Code by default)",
    help_arg_install_global: "Install in the global settings instead of the current project",
    help_arg_install_harness: "Target harness: claude, gemini or kimi",
    help_cmd_uninstall: "Remove the not-goldfish hooks from a harness (inverse of install; the database and captured memories stay intact)",
    help_arg_uninstall_global: "Remove from the global settings instead of the current project",
    help_arg_uninstall_harness: "Target harness: claude, gemini or kimi",
    help_cmd_completions: "Print the `ng` shell-completion script for the given shell (e.g. `ng completions bash >> ~/.bashrc` or the completions dir)",
    help_arg_completions_shell: "Target shell: bash, zsh, fish, elvish or powershell",
    help_cmd_search: "Search the persistent memory",
    help_arg_search_query: "Search terms",
    help_arg_search_here: "Limit to the current project",
    help_arg_search_limit: "Maximum number of results",
    help_arg_search_semantic: "Hybrid search: recall via FTS, rerank by semantic similarity",
    help_arg_search_json: "Stable JSON output (for scripts)",
    help_cmd_clear: "Lossless procedural hygiene: collapses cold items of the active session into recoverable stubs (with backup). Nothing is lost — everything stays in the database and comes back with `ng search`",
    help_arg_clear_file: "Path of the transcript to clean (default: most recent Claude Code session of this project)",
    help_arg_clear_target_tokens: "Target token budget for the live context (cold items above it become stubs)",
    help_arg_clear_dry_run: "Only show what would be collapsed, without rewriting",
    help_cmd_status: "Show the state of the database and the daemon",
    help_arg_status_json: "Stable JSON output (for scripts)",
    help_cmd_daemon: "Start the daemon in the foreground (use a service manager for background)",
    help_cmd_ui: "Open the context-management web UI (starts the daemon in the background if needed)",
    help_cmd_doctor: "Environment diagnostics: binaries, daemon, database, hooks, UI, backlog",
    help_cmd_mcp: "MCP integrations (servers registered per command)",
    help_cmd_mcp_install_browser_use: "Register the browser-use MCP server (requires `uvx` installed)",
    help_arg_mcp_ibu_harness: "Target harness: claude or codex",
    help_arg_mcp_ibu_global: "(Claude Code only) install in the global settings instead of the current project",
    help_cmd_sync: "Sync universal personas (~/.not-goldfish/personas) to each harness's subagent format",
    help_arg_sync_personas_dir: "Source personas directory (default: ~/.not-goldfish/personas)",
    help_arg_sync_global: "Sync into ~/.claude/agents instead of the current project",
    help_cmd_dispatch: "Suggest a model/category for a prompt (smart dispatch)",
    help_arg_dispatch_prompt: "Prompt to classify (ignored with --init)",
    help_arg_dispatch_init: "Write the default dispatch.toml (commented) to edit",
    help_cmd_wisdom: "Show the wisdom graph (entities/decisions extracted from sessions)",
    help_arg_wisdom_here: "Limit to the current project",
    help_arg_wisdom_md: "Export as Markdown (to paste into CLAUDE.md/AGENTS.md)",
    help_arg_wisdom_json: "Stable JSON output (for scripts)",
    help_arg_wisdom_rebuild: "Rebuild the graph from scratch by re-ingesting the whole history with the current rules (entities/relations are derived; events untouched)",
    help_cmd_memory: "Inspect and edit not-goldfish's own memory (hiding is reversible — nothing is deleted)",
    help_cmd_memory_list: "List stored memories (most recent first)",
    help_arg_memory_list_here: "Limit to the current project",
    help_arg_memory_list_all: "Include hidden memories (marked with [hidden])",
    help_arg_memory_list_limit: "Maximum number of memories",
    help_cmd_memory_hide: "Hide a memory from search/injection (reversible, nothing is deleted)",
    help_arg_memory_hide_id: "Memory id (see `ng memory list`)",
    help_cmd_memory_unhide: "Restore a hidden memory to search/injection",
    help_arg_memory_unhide_id: "Memory id",
    help_cmd_memory_add: "Add a memory manually",
    help_arg_memory_add_content: "Memory content",
    help_arg_memory_add_project: "Project to associate it with (default: empty = global)",
    help_arg_memory_add_tags: "Tags (space-separated)",
    help_cmd_gain: "Accumulated benefit since adoption: captures, injections, hygiene",
    help_arg_gain_here: "Limit to the current project (cwd)",
    help_arg_gain_json: "Stable JSON output (for scripts)",
    help_arg_gain_since: "Only count from this date (YYYY-MM-DD)",
    help_cmd_saver: "External savers (pluggable token compressors): init, list and the bench measurement gate — all OFF by default",
    help_cmd_saver_init: "Write the commented ~/.not-goldfish/savers.toml (never overwrites)",
    help_cmd_saver_list: "List the configured savers and the state of the measurement gate",
    help_arg_saver_list_json: "Stable JSON output (for scripts)",
    help_cmd_saver_bench: "Measure a saver against real tool_outputs from the database and promote it to \"trusted\" only if it is net-positive over the native stub",
    help_arg_saver_bench_name: "Saver name (as defined in the global savers.toml)",
    help_arg_saver_bench_sample: "Maximum number of real events in the sample",
    help_cmd_sync_context: "(Re)generate .ng/ — a committable projection of this project's memory",
    help_arg_sync_context_init: "Also create a commented .ng/config.toml (never overwrites an existing one)",
    help_arg_sync_context_dir: "Project directory (default: cwd)",
};

static PT: Msgs = Msgs {
    // ---- compartilhado -------------------------------------------------
    db_missing: "banco não existe ainda ({path}). Rode `ng install` e use uma sessão para capturar memória.",
    ngd_not_found: "ngd não encontrado (compile com `cargo build --release` ou coloque no PATH)",

    // ---- status --------------------------------------------------------
    status_banner: "not-goldfish · status",
    status_data: "dados",
    status_db: "banco",
    status_db_fmt: "{events} eventos · {sessions} sessões · ~{tokens} tokens · {mib} MiB",
    status_db_absent: "(ainda não criado)",
    status_daemon: "daemon",
    status_daemon_running: "rodando",
    status_daemon_stopped: "parado (captura cai no fallback direto)",

    // ---- search --------------------------------------------------------
    search_banner: "not-goldfish · busca",
    search_no_results: "nenhum resultado para: {query}",
    search_session: "sessão",

    // ---- gain ----------------------------------------------------------
    gain_banner: "not-goldfish · ganho acumulado",
    gain_scope_project: "— projeto {proj}",
    gain_scope_global: "— global (todos os projetos)",
    gain_counting_from: "(contando a partir de {date})",
    gain_using_since: "usando desde",
    gain_using_since_value: "{date} ({days} dias)",
    gain_using_since_none: "— (nenhum evento no escopo)",
    gain_sec_memory: "memória",
    gain_events_captured: "eventos capturados",
    gain_sessions_tracked: "sessões acompanhadas",
    gain_tokens_stored: "tokens armazenados",
    gain_sec_inject: "injeção proativa (custo declarado, não economia)",
    gain_prompts_served: "prompts atendidos",
    gain_memories_served: "memórias servidas",
    gain_tokens_injected: "tokens injetados",
    gain_sec_hygiene: "higiene de contexto (economia real, contada 1x por eviction)",
    gain_passes_precompact: "passadas (PreCompact)",
    gain_passes_clear: "passadas (ng clear)",
    gain_items_stubbed: "itens stubados",
    gain_tokens_saved: "tokens economizados",
    gain_net_ratio: "proporção líquida",
    gain_no_data: "    — (ainda sem dados; sinal passa a ser registrado a partir desta versão)",
    gain_footer_estimates: "Estimativas ×4 bytes/token. Injeção é custo declarado, não economia.",
    gain_footer_source: "Fonte: gain_ledger + events no banco local ({db}).",
    gain_since_invalid: "--since inválido: {s} (formato esperado: YYYY-MM-DD)",

    // ---- wisdom --------------------------------------------------------
    wisdom_json_md_exclusive: "--json e --md são mutuamente exclusivos",
    wisdom_rebuilding: "reconstruindo grafo…",
    wisdom_rebuilt: "grafo reconstruído: {n} eventos re-ingeridos",
    wisdom_banner: "not-goldfish · grafo de sabedoria",
    wisdom_scope_here: "— projeto atual",
    wisdom_scope_global: "— global",
    wisdom_empty: "grafo de sabedoria ainda vazio — capture mais sessões para ele se popular",
    wisdom_weight: "peso",
    wisdom_no_neighbors: "   (sem vizinhos)",
    wisdom_score: "score",

    // ---- install -------------------------------------------------------
    install_hook_not_found: "ng-hook não encontrado (compile com `cargo build --release` ou coloque no PATH)",
    install_backup: "backup:",
    install_hooks_installed: "hooks instalados em {path} ({label})",
    install_events: "eventos: {list}",
    install_db_init_warn: "aviso: não foi possível inicializar o banco agora ({err}); a primeira captura vai criá-lo",
    install_hint: "dica: rode `ng daemon` (ou deixe o fallback direto no SQLite fazer o trabalho)",

    // ---- ui ------------------------------------------------------------
    ui_starting_daemon: "iniciando daemon…",
    ui_starting: "iniciando {path}",
    ui_daemon_failed: "o daemon não subiu — rode `ng daemon` num terminal para ver o erro",

    // ---- daemon --------------------------------------------------------
    daemon_running: "executando {path}",
    daemon_exited: "ngd saiu com {status}",

    // ---- memory --------------------------------------------------------
    mem_empty: "nenhuma memória armazenada neste escopo",
    mem_flag_hidden: " [oculta]",
    mem_flag_manual: " [manual]",
    mem_hidden_word: "oculta",
    mem_hide_ok: "memória {id} {word} (reversível com `ng memory unhide {raw}`)",
    mem_hide_none: "nenhuma memória visível com id {id} (talvez já oculta ou inexistente)",
    mem_unhide_word: "restaurada",
    mem_unhide_ok: "memória {id} {word} para a busca/injeção",
    mem_unhide_none: "nenhuma memória com id {id}",
    mem_add_empty: "conteúdo vazio — passe o texto da memória",
    mem_add_word: "adicionada",
    mem_add_ok: "memória {id} {word}",

    // ---- sync ----------------------------------------------------------
    sync_seeded: "nenhuma persona em {path} — personas padrão (ceo, pm, dev) escritas ali",
    sync_synced: "{count} personas sincronizadas em {path}",
    sync_opencode: "opencode.json também sincronizado (backup: {path})",

    // ---- doctor --------------------------------------------------------
    doc_banner: "not-goldfish · doctor",
    doc_sec_binaries: "binários",
    doc_sec_daemon: "daemon",
    doc_sec_database: "banco de dados",
    doc_sec_hooks: "hooks",
    doc_sec_ui: "interface web",
    doc_sec_embeddings: "embeddings",
    doc_bin_found: "binário {name} encontrado em {path}",
    doc_bin_missing: "binário {name} não encontrado — rode `cargo build --release` ou coloque {name} no PATH",
    doc_daemon_ok: "daemon respondendo em {socket} ({ms}ms)",
    doc_daemon_refused: "daemon conectou mas recusou escrita em {socket} — verifique os logs do `ngd`",
    doc_daemon_down: "daemon não responde em {socket} — rode `ng daemon` (captura ainda funciona via fallback direto)",
    doc_db_absent: "banco ainda não existe em {path} — rode `ng install` e use uma sessão para capturar",
    doc_db_open_fail: "não foi possível abrir o banco em {path}: {err}",
    doc_db_stats: "banco em {path}: {events} eventos · {sessions} sessões · ~{tokens} tokens · {mib} MiB",
    doc_db_stats_fail: "banco abriu mas stats() falhou ({err}) — pode estar corrompido, considere restaurar de um backup",
    doc_journal_fail: "não foi possível checar journal_mode: {err}",
    doc_quick_fail: "não foi possível rodar quick_check: {err}",
    doc_fts_fail: "não foi possível checar o índice FTS: {err}",
    doc_journal_ok: "journal_mode = {mode}",
    doc_journal_warn: "journal_mode = {mode} (esperado wal) — abra o banco uma vez com `ng daemon` para reativar WAL",
    doc_dbsize_warn: "banco com {gib} GiB — considere `ng clear` para higiene de transcripts antigos (nada é deletado do banco)",
    doc_quick_ok_timed: "integridade (quick_check) ok em {s}s",
    doc_quick_ok: "integridade (quick_check) ok",
    doc_quick_fail_row: "quick_check reportou: {row} — banco corrompido; restaure de backup ou copie {db} para análise antes de qualquer escrita",
    doc_fts_ok: "índice FTS íntegro (integrity-check)",
    doc_fts_busy: "não deu para checar o índice FTS (banco ocupado) — pare o daemon e rode de novo",
    doc_fts_corrupt: "índice FTS dessincronizado/corrompido ({err}) — busca e injeção não são confiáveis; restaure de um backup",
    doc_hooks_label_project: "projeto",
    doc_hooks_label_global: "global",
    doc_hooks_covered: "hooks ({label}) em {path}: {events}",
    doc_hooks_dangling: "o hook em {path} aponta para `{hook}` que não existe — rode `ng install` de novo",
    doc_hooks_precompact_missing: "hook {event} ausente em {path} — higiene automática (NG_AUTO_HYGIENE) não dispara sem ele, rode `ng install` de novo após atualizar",
    doc_hooks_invalid_json: "{path} existe mas não é JSON válido — não foi possível checar hooks",
    doc_hooks_none: "nenhum hook ng-hook instalado — rode `ng install` (ou `ng install --global`)",
    doc_ui_ok: "UI respondendo em http://127.0.0.1:{port}",
    doc_ui_warn: "UI não responde em 127.0.0.1:{port} — rode `ng ui` para abrir",
    doc_embed_warn: "{count} eventos sem embedding — enriquecimento atrasado, confira se `ngd` está rodando",
    doc_embed_ok: "{count} eventos aguardando embedding (dentro do normal)",

    // ---- clap help (about + argumentos) --------------------------------
    help_about: "not-goldfish: memória universal para harnesses de IA",
    help_cmd_install: "Registra os hooks do not-goldfish num harness (Claude Code por padrão)",
    help_arg_install_global: "Instalar no settings global em vez do projeto atual",
    help_arg_install_harness: "Harness alvo: claude, gemini ou kimi",
    help_cmd_uninstall: "Remove os hooks do not-goldfish de um harness (inverso do install; banco e memórias capturadas ficam intactos)",
    help_arg_uninstall_global: "Remover do settings global em vez do projeto atual",
    help_arg_uninstall_harness: "Harness alvo: claude, gemini ou kimi",
    help_cmd_completions: "Imprime o script de autocompletar do `ng` para o shell dado (ex.: `ng completions bash >> ~/.bashrc` ou o dir de completions)",
    help_arg_completions_shell: "Shell alvo: bash, zsh, fish, elvish ou powershell",
    help_cmd_search: "Busca na memória persistente",
    help_arg_search_query: "Termos de busca",
    help_arg_search_here: "Limitar ao projeto atual",
    help_arg_search_limit: "Máximo de resultados",
    help_arg_search_semantic: "Busca híbrida: recall por FTS, rerank por similaridade semântica",
    help_arg_search_json: "Saída JSON estável (para scripts)",
    help_cmd_clear: "Higiene procedural lossless: colapsa itens frios da sessão ativa em stubs recuperáveis (com backup). Nada é perdido — tudo continua no banco e volta com `ng search`",
    help_arg_clear_file: "Caminho do transcript a limpar (padrão: sessão Claude Code mais recente deste projeto)",
    help_arg_clear_target_tokens: "Orçamento de tokens alvo do contexto vivo (itens frios acima disso viram stub)",
    help_arg_clear_dry_run: "Só mostra o que seria colapsado, sem reescrever",
    help_cmd_status: "Mostra o estado do banco e do daemon",
    help_arg_status_json: "Saída JSON estável (para scripts)",
    help_cmd_daemon: "Inicia o daemon em foreground (use um service manager para background)",
    help_cmd_ui: "Abre a UI web de gerenciamento de contexto (inicia o daemon em background se necessário)",
    help_cmd_doctor: "Diagnóstico do ambiente: binários, daemon, banco, hooks, UI, backlog",
    help_cmd_mcp: "Integrações MCP (servidores registrados por comando)",
    help_cmd_mcp_install_browser_use: "Registra o servidor MCP browser-use (requer `uvx` instalado)",
    help_arg_mcp_ibu_harness: "Harness alvo: claude ou codex",
    help_arg_mcp_ibu_global: "(Só Claude Code) instalar no settings global em vez do projeto atual",
    help_cmd_sync: "Sincroniza personas universais (~/.not-goldfish/personas) para o formato de subagente de cada harness",
    help_arg_sync_personas_dir: "Diretório de personas de origem (padrão: ~/.not-goldfish/personas)",
    help_arg_sync_global: "Sincronizar em ~/.claude/agents em vez do projeto atual",
    help_cmd_dispatch: "Sugere modelo/categoria para um prompt (dispatch inteligente)",
    help_arg_dispatch_prompt: "Prompt a classificar (ignorado com --init)",
    help_arg_dispatch_init: "Escreve o dispatch.toml padrão (comentado) para editar",
    help_cmd_wisdom: "Mostra o grafo de sabedoria (entidades/decisões extraídas das sessões)",
    help_arg_wisdom_here: "Limitar ao projeto atual",
    help_arg_wisdom_md: "Exporta em Markdown (para colar em CLAUDE.md/AGENTS.md)",
    help_arg_wisdom_json: "Saída JSON estável (para scripts)",
    help_arg_wisdom_rebuild: "Reconstrói o grafo do zero re-ingerindo todo o histórico com as regras atuais (entities/relations são derivadas; events intocada)",
    help_cmd_memory: "Inspeciona e edita a memória própria do not-goldfish (ocultar é reversível — nada é apagado)",
    help_cmd_memory_list: "Lista memórias armazenadas (mais recentes primeiro)",
    help_arg_memory_list_here: "Limitar ao projeto atual",
    help_arg_memory_list_all: "Incluir memórias ocultas (marcadas com [oculta])",
    help_arg_memory_list_limit: "Máximo de memórias",
    help_cmd_memory_hide: "Oculta uma memória da busca/injeção (reversível, nada é apagado)",
    help_arg_memory_hide_id: "Id da memória (veja `ng memory list`)",
    help_cmd_memory_unhide: "Restaura uma memória oculta para a busca/injeção",
    help_arg_memory_unhide_id: "Id da memória",
    help_cmd_memory_add: "Adiciona uma memória manualmente",
    help_arg_memory_add_content: "Conteúdo da memória",
    help_arg_memory_add_project: "Projeto ao qual associar (padrão: vazio = global)",
    help_arg_memory_add_tags: "Tags (separadas por espaço)",
    help_cmd_gain: "Benefício acumulado desde a adoção: capturas, injeções, higiene",
    help_arg_gain_here: "Limitar ao projeto atual (cwd)",
    help_arg_gain_json: "Saída JSON estável (para scripts)",
    help_arg_gain_since: "Só contar a partir desta data (YYYY-MM-DD)",
    help_cmd_saver: "Savers externos (compressores de token plugáveis): init, list e o gate de medição bench — tudo OFF por default",
    help_cmd_saver_init: "Escreve o ~/.not-goldfish/savers.toml comentado (nunca sobrescreve)",
    help_cmd_saver_list: "Lista os savers configurados e o estado do gate de medição",
    help_arg_saver_list_json: "Saída JSON estável (para scripts)",
    help_cmd_saver_bench: "Mede um saver contra tool_outputs reais do banco e promove a \"trusted\" só se for líquido-positivo sobre o stub nativo",
    help_arg_saver_bench_name: "Nome do saver (como definido no savers.toml global)",
    help_arg_saver_bench_sample: "Máximo de eventos reais na amostra",
    help_cmd_sync_context: "(Re)gera .ng/ — projeção commitável da memória deste projeto",
    help_arg_sync_context_init: "Cria também .ng/config.toml comentado (nunca sobrescreve um existente)",
    help_arg_sync_context_dir: "Diretório do projeto (padrão: cwd)",
};

#[cfg(test)]
mod tests {
    use super::*;

    // ---- detecção (núcleo puro, sem tocar em env) ----------------------

    #[test]
    fn ng_lang_takes_precedence_over_locale() {
        // NG_LANG=en vence LANG=pt_BR, e NG_LANG=pt vence LANG=en_US.
        assert_eq!(
            detect_from(Some("en"), None, None, Some("pt_BR.UTF-8")),
            Lang::En
        );
        assert_eq!(
            detect_from(Some("pt"), None, None, Some("en_US.UTF-8")),
            Lang::Pt
        );
    }

    #[test]
    fn ng_lang_is_case_insensitive_and_prefix_tolerant() {
        assert_eq!(detect_from(Some("EN"), None, None, None), Lang::En);
        assert_eq!(detect_from(Some("PT"), None, None, None), Lang::Pt);
        assert_eq!(detect_from(Some("pt_BR"), None, None, None), Lang::Pt);
        assert_eq!(detect_from(Some(" en "), None, None, None), Lang::En);
    }

    #[test]
    fn unknown_ng_lang_falls_through_to_locale() {
        // Um NG_LANG que não é en/pt não força En — o locale decide.
        assert_eq!(detect_from(Some("fr"), None, None, Some("pt_BR")), Lang::Pt);
        assert_eq!(detect_from(Some("fr"), None, None, Some("en_US")), Lang::En);
    }

    #[test]
    fn lang_pt_br_detects_portuguese() {
        assert_eq!(detect_from(None, None, None, Some("pt_BR.UTF-8")), Lang::Pt);
    }

    #[test]
    fn lang_en_us_detects_english() {
        assert_eq!(detect_from(None, None, None, Some("en_US.UTF-8")), Lang::En);
    }

    #[test]
    fn lc_all_wins_over_lc_messages_and_lang() {
        // Precedência POSIX: LC_ALL manda, mesmo com LANG divergente.
        assert_eq!(
            detect_from(None, Some("pt_BR.UTF-8"), Some("en_US"), Some("en_US")),
            Lang::Pt
        );
        assert_eq!(
            detect_from(None, Some("en_US.UTF-8"), Some("pt_BR"), Some("pt_BR")),
            Lang::En
        );
    }

    #[test]
    fn lc_messages_used_when_lc_all_unset() {
        assert_eq!(
            detect_from(None, None, Some("pt_BR"), Some("en_US")),
            Lang::Pt
        );
    }

    #[test]
    fn empty_locale_values_are_skipped() {
        // LC_ALL="" não conta como locale efetivo — cai no próximo setado.
        assert_eq!(detect_from(None, Some(""), None, Some("pt_BR")), Lang::Pt);
    }

    #[test]
    fn nothing_set_defaults_to_english() {
        assert_eq!(detect_from(None, None, None, None), Lang::En);
    }

    // ---- catálogo ------------------------------------------------------

    #[test]
    fn for_lang_returns_the_matching_catalog() {
        assert_eq!(Msgs::for_lang(Lang::En).status_data, "data");
        assert_eq!(Msgs::for_lang(Lang::Pt).status_data, "dados");
    }

    #[test]
    fn fill_replaces_named_placeholders() {
        assert_eq!(
            fill("{a} then {b}", &[("{a}", &"x"), ("{b}", &42)]),
            "x then 42"
        );
    }

    #[test]
    fn fill_is_order_independent_between_languages() {
        // O mesmo par de args serve a templates com ordem de palavras
        // diferente — a chave do i18n é a substituição por nome.
        let args: &[(&str, &dyn Display)] = &[("{n}", &3), ("{path}", &"/x")];
        assert_eq!(fill("{n} in {path}", args), "3 in /x");
        assert_eq!(fill("{path} has {n}", args), "/x has 3");
    }
}
