//! ng-stress: extreme-scale stress harness for not-goldfish's invariants.
//!
//! Measures, on throwaway databases under a temp dir (never the user's real
//! database), the product invariants from CLAUDE.md under 10k/100k/500k/1M
//! events:
//!
//! 1. ingestion scale (time, events/s, final .db size);
//! 2. hook hot path (`build_injection_readonly`) p50/p95 vs the ~5ms budget;
//! 3. FTS search latency for common vs rare terms;
//! 4. atomic rewrite of ~100MB/~500MB JSONL transcripts;
//! 5. `PRAGMA integrity_check`/`quick_check` after the heavy scenarios;
//! 6. wisdom-graph rebuild time at 100k+ events.
//!
//! Every number printed here comes from a real run — nothing is extrapolated.
//! See `docs/benchmarks/stress-test.md` for methodology and results.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ng_core::{lex, Event, Store};
use ng_hook::inject::build_injection_readonly;
use ng_sessions::rewrite::rewrite_jsonl;
use serde::Serialize;

/// Event volumes per isolated database, smallest first.
const SIZES: &[usize] = &[10_000, 100_000, 500_000, 1_000_000];
/// Timed repetitions per latency measurement (p50/p95 over these).
const RUNS: usize = 120;
/// Real hook hot-path budget per prompt (CLAUDE.md invariant).
const HOT_PATH_BUDGET_MS: f64 = 5.0;

/// Same deterministic template shape as `tests/latency_floor.rs`, so the
/// corpus (and the IDF distribution the hook's selective FTS sees) matches
/// what the latency gate already measures.
const TEMPLATES: &[&str] = &[
    "corrigir bug de autenticação no login: token JWT expira antes do refresh (evento {i})",
    "cache redis invalidado com TTL de 300s após deploy do serviço de sessão {i}",
    "docker build falhou na camada de dependências; layer cache não reaproveitado {i}",
    "migração sqlite: ALTER TABLE events ADD COLUMN tokens_est INTEGER NOT NULL {i}",
    "runtime tokio: task async trava aguardando await em canal mpsc fechado {i}",
];
/// Rare event interleaved every [`RARE_EVERY`] events (~0.2% of the corpus)
/// so its terms survive the IDF pruning of `selective_fts_query` and the
/// measured hot path does real FTS work instead of degrading to an empty
/// query.
const RARE_TEMPLATE: &str =
    "deadlock no scheduler zephyr: mutex de quorum preso durante failover do raft {i}";
const RARE_EVERY: usize = 500;
/// Prompt with direct lexical overlap with the rare template.
const HOT_PROMPT: &str = "deadlock de mutex no quorum do raft durante failover do scheduler";
/// Common-term query (hits ~20% of the corpus) vs rare-term query.
const COMMON_QUERY: &str = "autenticação login token";
const RARE_QUERY: &str = "deadlock zephyr quorum raft";

/// Transcript sizes for the rewrite scenario (bytes, approximate).
const REWRITE_SIZES: &[(usize, &str)] = &[(100_000_000, "~100MB"), (500_000_000, "~500MB")];

#[derive(Serialize)]
struct Percentiles {
    runs: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Serialize)]
struct IngestResult {
    n: usize,
    seconds: f64,
    events_per_sec: f64,
    db_bytes: u64,
}

#[derive(Serialize)]
struct HotPathResult {
    n: usize,
    latencies: Percentiles,
    /// Fraction of runs where the injection actually found memories (a
    /// 0.0 here would mean we measured a degenerate empty-search path).
    found_ratio: f64,
    under_budget_p95: bool,
}

#[derive(Serialize)]
struct SearchResult {
    n: usize,
    common: Percentiles,
    rare: Percentiles,
}

#[derive(Serialize)]
struct RewriteResult {
    label: String,
    file_bytes: u64,
    lines: usize,
    seconds: f64,
    peak_dir_bytes: u64,
    backup_created: bool,
    backup_matches_original: bool,
    untouched_lines_intact: bool,
    line_count_preserved: bool,
}

#[derive(Serialize)]
struct IntegrityResult {
    db: String,
    n: usize,
    integrity_check: String,
    quick_check: String,
}

#[derive(Serialize)]
struct GraphResult {
    n: usize,
    seconds: f64,
    events_ingested: usize,
}

#[derive(Serialize)]
struct StressReport {
    started_utc: String,
    temp_dir: String,
    ingest: Vec<IngestResult>,
    hot_path: Vec<HotPathResult>,
    search: Vec<SearchResult>,
    rewrite: Vec<RewriteResult>,
    integrity: Vec<IntegrityResult>,
    graph: Vec<GraphResult>,
}

fn percentiles(samples_ms: &mut [f64]) -> Percentiles {
    samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("latencies are finite"));
    let pick = |p: usize| samples_ms[(samples_ms.len() * p / 100).min(samples_ms.len() - 1)];
    Percentiles {
        runs: samples_ms.len(),
        min_ms: *samples_ms.first().unwrap_or(&0.0),
        p50_ms: pick(50),
        p95_ms: pick(95),
        max_ms: *samples_ms.last().unwrap_or(&0.0),
    }
}

fn event_at(i: usize) -> Event {
    let template = if i.is_multiple_of(RARE_EVERY) {
        RARE_TEMPLATE
    } else {
        TEMPLATES[i % TEMPLATES.len()]
    };
    let content = template.replace("{i}", &i.to_string());
    Event {
        session_id: format!("sess-{}", i % 32),
        project: "/tmp/not-goldfish-stress".to_string(),
        harness: "claude-code".to_string(),
        kind: "prompt".to_string(),
        tags: lex::extract_tags(&content),
        content,
        meta: None,
        created_at: 1_700_000_000 + (i as i64) * 60,
    }
}

/// Seed `n` events through the real capture path (`Store::insert_event`,
/// one autocommit per event — the same cadence the daemon pays per captured
/// event). Returns wall time and the final on-disk size of the main .db
/// file after a WAL checkpoint.
fn scenario_ingest(dir: &Path, n: usize) -> (PathBuf, IngestResult) {
    let db = dir.join(format!("stress-{n}.db"));
    let store = Store::open(&db).expect("open stress db");
    let start = Instant::now();
    for i in 0..n {
        store.insert_event(&event_at(i)).expect("insert event");
    }
    let seconds = start.elapsed().as_secs_f64();
    let (count, _, _) = store.stats().expect("stats");
    assert_eq!(count as usize, n, "seeded event count mismatch");
    drop(store);

    // Checkpoint so the reported size is the real main-file size, not an
    // artifact of whatever still sits in the WAL.
    let conn = rusqlite::Connection::open(&db).expect("reopen for checkpoint");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint");
    drop(conn);
    let db_bytes = fs::metadata(&db).expect("db metadata").len();

    (
        db,
        IngestResult {
            n,
            seconds,
            events_per_sec: n as f64 / seconds,
            db_bytes,
        },
    )
}

/// The hook's real per-prompt read path: `build_injection_readonly` opens
/// the database read-only and runs the selective FTS + bm25 + dedup +
/// formatting pipeline on every call — exactly what `ng-hook` pays per
/// prompt. Budget: <5ms (p50 AND p95 reported; p95 is the gate here).
fn scenario_hot_path(db: &Path, n: usize) -> HotPathResult {
    let mut samples = Vec::with_capacity(RUNS);
    let mut found = 0usize;
    for _ in 0..RUNS {
        let start = Instant::now();
        let out = build_injection_readonly(db, HOT_PROMPT, "stress-probe-session");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        if out.is_some() {
            found += 1;
        }
    }
    let latencies = percentiles(&mut samples);
    HotPathResult {
        n,
        under_budget_p95: latencies.p95_ms < HOT_PATH_BUDGET_MS,
        latencies,
        found_ratio: found as f64 / RUNS as f64,
    }
}

/// FTS search latency (`Store::search`, the `ng search` path) for a common
/// term set (~20% document frequency) and a rare one (~0.2%).
fn scenario_search(db: &Path, n: usize) -> SearchResult {
    let store = Store::open_readonly(db).expect("open readonly for search");
    let mut common = Vec::with_capacity(RUNS);
    let mut rare = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        let hits = store.search(COMMON_QUERY, None, 10).expect("common search");
        common.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(!hits.is_empty(), "common query must hit in a seeded corpus");

        let start = Instant::now();
        let hits = store.search(RARE_QUERY, None, 10).expect("rare search");
        rare.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(!hits.is_empty(), "rare query must hit in a seeded corpus");
    }
    SearchResult {
        n,
        common: percentiles(&mut common),
        rare: percentiles(&mut rare),
    }
}

/// One JSONL transcript line of roughly `target` bytes.
fn transcript_line(i: usize, target: usize) -> String {
    let mut line = format!(
        "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"output do comando numero {i}: "
    );
    while line.len() < target - 16 {
        let _ = write!(line, "palavra{i} ");
    }
    line.push_str("\"}]}}");
    line
}

/// Samples the total bytes of a directory every 25ms while `stop` is unset,
/// recording the peak — during a rewrite the original + backup + tmp file
/// coexist, and this captures that transient 3x footprint.
fn spawn_disk_sampler(
    dir: PathBuf,
) -> (Arc<AtomicBool>, Arc<AtomicU64>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));
    let (stop2, peak2) = (stop.clone(), peak.clone());
    let handle = std::thread::spawn(move || {
        while !stop2.load(Ordering::Relaxed) {
            let total: u64 = fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter_map(|e| e.metadata().ok())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0);
            peak2.fetch_max(total, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    (stop, peak, handle)
}

/// Rewrite a giant transcript: replace exactly one line with a stub and
/// verify the safety invariants — backup exists and is byte-identical to
/// the original, the rename landed, and every untouched line survived
/// byte-for-byte (replacement only, never deletion).
fn scenario_rewrite(dir: &Path, target_bytes: usize, label: &str) -> RewriteResult {
    let path = dir.join(format!("transcript-{label}.jsonl"));
    let mut lines = Vec::new();
    let mut total = 0usize;
    let mut i = 0usize;
    while total < target_bytes {
        let line = transcript_line(i, 1024);
        total += line.len() + 1;
        lines.push(line);
        i += 1;
    }
    {
        let mut f = fs::File::create(&path).expect("create transcript");
        use std::io::Write as _;
        for line in &lines {
            writeln!(f, "{line}").expect("write transcript line");
        }
        f.sync_all().expect("sync transcript");
    }
    let file_bytes = fs::metadata(&path).expect("transcript metadata").len();
    let line_count = lines.len();

    let (stop, peak, sampler) = spawn_disk_sampler(dir.to_path_buf());
    let stub = "{\"type\":\"summary\",\"stub\":\"[ng: 1 item substituído por stub]\"}".to_string();
    let start = Instant::now();
    let backup = rewrite_jsonl(&path, &[], &[(1, stub.clone())]).expect("rewrite_jsonl");
    let seconds = start.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    sampler.join().expect("sampler join");
    let peak_dir_bytes = peak.load(Ordering::Relaxed);

    let backup_created = backup.exists();
    let backup_bytes = fs::read(&backup).expect("read backup");
    let mut expected = lines.join("\n").into_bytes();
    expected.push(b'\n');
    let backup_matches_original = backup_bytes == expected;
    drop(expected);
    drop(backup_bytes);

    // Verify the rewritten file: same line count, line 1 is the stub, every
    // other line byte-identical to the original.
    let after = fs::read_to_string(&path).expect("read rewritten transcript");
    let mut after_lines = after.lines();
    let stub_ok = after_lines.next().is_some_and(|l| l == stub);
    let mut untouched_ok = stub_ok;
    let mut after_count = if stub_ok { 1 } else { 0 };
    for (idx, line) in after_lines.enumerate() {
        after_count += 1;
        if lines.get(idx + 1).map(String::as_str) != Some(line) {
            untouched_ok = false;
            break;
        }
    }
    let line_count_preserved = after_count == line_count;

    RewriteResult {
        label: label.to_string(),
        file_bytes,
        lines: line_count,
        seconds,
        peak_dir_bytes,
        backup_created,
        backup_matches_original,
        untouched_lines_intact: untouched_ok,
        line_count_preserved,
    }
}

/// `PRAGMA integrity_check` + `PRAGMA quick_check` over a direct rusqlite
/// connection — both must return "ok" after the heavy scenarios.
fn scenario_integrity(db: &Path, n: usize) -> IntegrityResult {
    let conn = rusqlite::Connection::open(db).expect("open for integrity");
    let integrity_check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity_check");
    let quick_check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .expect("quick_check");
    IntegrityResult {
        db: db
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
        n,
        integrity_check,
        quick_check,
    }
}

/// Wisdom-graph rebuild (`Store::graph_rebuild`) over a large corpus.
fn scenario_graph(db: &Path, n: usize) -> GraphResult {
    let store = Store::open(db).expect("open for graph rebuild");
    let start = Instant::now();
    let events_ingested = store.graph_rebuild().expect("graph rebuild");
    let seconds = start.elapsed().as_secs_f64();
    GraphResult {
        n,
        seconds,
        events_ingested,
    }
}

fn main() -> anyhow::Result<()> {
    // Isolated temp dir per run — the user's real database and the daemon
    // on :4949 are never touched. Kept after the run for inspection.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("ng-stress-{}-{unique}", std::process::id()));
    fs::create_dir_all(&dir)?;
    eprintln!(
        "ng-stress: diretório isolado de trabalho: {}",
        dir.display()
    );

    let mut report = StressReport {
        started_utc: format!("{unique}"),
        temp_dir: dir.display().to_string(),
        ingest: Vec::new(),
        hot_path: Vec::new(),
        search: Vec::new(),
        rewrite: Vec::new(),
        integrity: Vec::new(),
        graph: Vec::new(),
    };

    // Scenarios 1+2+3 share the same per-size databases: seed once, then
    // measure the hot path and FTS search on that same volume.
    let mut dbs: Vec<(usize, PathBuf)> = Vec::new();
    for &n in SIZES {
        eprintln!("ng-stress: [1] ingestão {n} eventos...");
        let (db, ingest) = scenario_ingest(&dir, n);
        eprintln!(
            "ng-stress:   -> {:.1}s ({:.0} ev/s), db = {:.1} MB",
            ingest.seconds,
            ingest.events_per_sec,
            ingest.db_bytes as f64 / 1e6,
        );
        report.ingest.push(ingest);

        eprintln!("ng-stress: [2] hot path do hook sobre {n} eventos ({RUNS} runs)...");
        let hot = scenario_hot_path(&db, n);
        eprintln!(
            "ng-stress:   -> p50 {:.2}ms p95 {:.2}ms (encontrou memória em {:.0}% dos runs)",
            hot.latencies.p50_ms,
            hot.latencies.p95_ms,
            hot.found_ratio * 100.0,
        );
        report.hot_path.push(hot);

        if n >= 100_000 {
            eprintln!("ng-stress: [3] busca FTS sobre {n} eventos ({RUNS} runs/termo)...");
            let search = scenario_search(&db, n);
            eprintln!(
                "ng-stress:   -> comum p50 {:.2}ms p95 {:.2}ms | raro p50 {:.2}ms p95 {:.2}ms",
                search.common.p50_ms, search.common.p95_ms, search.rare.p50_ms, search.rare.p95_ms,
            );
            report.search.push(search);
        }
        dbs.push((n, db));
    }

    // Scenario 4: giant transcript rewrites (own subdir so the disk sampler
    // only sees the transcript + backup + tmp files).
    let rewrite_dir = dir.join("rewrite");
    fs::create_dir_all(&rewrite_dir)?;
    for &(size, label) in REWRITE_SIZES {
        eprintln!("ng-stress: [4] rewrite de transcript {label}...");
        let r = scenario_rewrite(&rewrite_dir, size, label);
        eprintln!(
            "ng-stress:   -> {:.1}s, {} linhas, pico de disco {:.0} MB, backup_ok={} intacto={}",
            r.seconds,
            r.lines,
            r.peak_dir_bytes as f64 / 1e6,
            r.backup_created && r.backup_matches_original,
            r.untouched_lines_intact && r.line_count_preserved,
        );
        report.rewrite.push(r);
        // Free the transcripts before the next size — 500MB + backup is
        // already measured; no need to keep both sizes on disk.
        fs::remove_dir_all(&rewrite_dir)?;
        fs::create_dir_all(&rewrite_dir)?;
    }

    // Scenario 5: integrity after the heavy writes.
    for (n, db) in &dbs {
        eprintln!("ng-stress: [5] integridade do banco de {n} eventos...");
        let r = scenario_integrity(db, *n);
        eprintln!(
            "ng-stress:   -> integrity_check={} quick_check={}",
            r.integrity_check, r.quick_check
        );
        report.integrity.push(r);
    }

    // Scenario 6: graph rebuild on the 100k+ databases.
    for (n, db) in &dbs {
        if *n < 100_000 {
            continue;
        }
        eprintln!("ng-stress: [6] rebuild do grafo sobre {n} eventos...");
        let g = scenario_graph(db, *n);
        eprintln!(
            "ng-stress:   -> {:.1}s ({} eventos ingeridos no grafo)",
            g.seconds, g.events_ingested
        );
        report.graph.push(g);
    }

    let out = dir.join("stress-results.json");
    fs::write(&out, serde_json::to_string_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("ng-stress: resultados JSON em {}", out.display());
    Ok(())
}
