//! Real concurrency test against the daemon's actual write pattern
//! (finding 17): several independent connections opened via `Store::open`
//! hammer the *same* tempfile SQLite database at once — direct inserts (the
//! writer thread's job), an embedding backlog worker (the enrich thread's
//! job), and a graph-weight bump (the UI's `/api/graph/bump` handler's
//! job) — all overlapping in time.
//!
//! This goes further than the pre-existing concurrent-*open* test: it
//! actually performs concurrent reads and writes and then asserts the
//! database ends up in a consistent state (exact event/session counts, zero
//! errors from any thread), which is the only way to catch a WAL/locking
//! regression that a mere "can multiple connections open without erroring"
//! check would miss.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ng_core::{Embedder, Event, HashEmbedder, Store};

const WRITER_THREADS: usize = 6;
const EVENTS_PER_THREAD: usize = 25;
/// How many enrich-worker passes to run concurrently with the writers, each
/// draining whatever backlog exists at that moment — mirrors `enrich::run`'s
/// poll loop, just without the `sleep` between passes so it actually
/// contends with the writers instead of mostly running after them.
const ENRICH_PASSES: usize = 40;
const GRAPH_BUMP_PASSES: usize = 40;

fn sample_event(thread_id: usize, seq: usize) -> Event {
    Event {
        session_id: format!("session-{thread_id}"),
        project: "/tmp/concurrency-proj".to_string(),
        harness: "claude-code".to_string(),
        kind: "prompt".to_string(),
        content: format!("evento {thread_id}-{seq} sobre src/main.rs e uma decisao qualquer"),
        tags: String::new(),
        meta: None,
        created_at: 1_700_000_000 + seq as i64,
    }
}

#[test]
fn writers_enrich_worker_and_graph_bump_run_concurrently_without_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("ng.db");

    // Pre-create the schema up front (mirrors main.rs: the daemon's own
    // writer thread is first to call Store::open at boot) so every spawned
    // thread below opens an already-initialized db instead of racing the
    // one-time WAL-conversion lock the store module's own doc comment
    // warns about.
    drop(Store::open(&db_path).expect("initial schema creation"));

    let errors = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Writer threads: exactly what `writer_loop` does per event, minus the
    // retry/dead-letter wrapper (that's covered in isolation by writer.rs's
    // own tests) — the point here is concurrent access to the shared file.
    for thread_id in 0..WRITER_THREADS {
        let db_path = db_path.clone();
        let errors = Arc::clone(&errors);
        handles.push(std::thread::spawn(move || {
            let store = match Store::open(&db_path) {
                Ok(s) => s,
                Err(_) => {
                    errors.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            };
            for seq in 0..EVENTS_PER_THREAD {
                if store.insert_event(&sample_event(thread_id, seq)).is_err() {
                    errors.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    // Enrich-worker-style thread: repeatedly drains the embedding backlog,
    // same as `enrich::enrich_batch` does every poll tick.
    {
        let db_path = db_path.clone();
        let errors = Arc::clone(&errors);
        handles.push(std::thread::spawn(move || {
            let store = match Store::open(&db_path) {
                Ok(s) => s,
                Err(_) => {
                    errors.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            };
            let embedder = HashEmbedder;
            for _ in 0..ENRICH_PASSES {
                match store.events_without_embedding(embedder.id(), 64) {
                    Ok(backlog) => {
                        for (id, content) in backlog {
                            let vec = embedder.embed(&content);
                            if store.upsert_embedding(id, embedder.id(), &vec).is_err() {
                                errors.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }));
    }

    // Graph-bump thread: same RW-open pattern as `api_graph_bump`. Bumping
    // an entity name that (mostly) doesn't exist yet is fine — the point is
    // exercising the write path concurrently, not the graph's ingestion
    // logic (already covered by ng-core's own tests).
    {
        let db_path = db_path.clone();
        let errors = Arc::clone(&errors);
        handles.push(std::thread::spawn(move || {
            let store = match Store::open(&db_path) {
                Ok(s) => s,
                Err(_) => {
                    errors.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            };
            for i in 0..GRAPH_BUMP_PASSES {
                if store
                    .bump_entity(&format!("src/file-{}.rs", i % 5), 0.1)
                    .is_err()
                {
                    errors.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    for handle in handles {
        handle.join().expect("thread should not panic");
    }

    assert_eq!(
        errors.load(Ordering::SeqCst),
        0,
        "no thread should observe an insert/upsert/bump error under concurrent access"
    );

    let verify = Store::open_readonly(&db_path).expect("final readonly open");
    let (events, sessions, _tokens_est) = verify.stats().expect("stats on the final db");
    assert_eq!(
        events,
        (WRITER_THREADS * EVENTS_PER_THREAD) as i64,
        "every insert from every writer thread must be durably persisted"
    );
    assert_eq!(sessions, WRITER_THREADS as i64, "each writer thread used a distinct session_id, so distinct-session count must match thread count");
}
