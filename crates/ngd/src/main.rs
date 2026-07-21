//! ngd: the not-goldfish daemon.
//!
//! Phase 1 scope: accept newline-delimited JSON events over a unix socket
//! and persist them through a single writer thread. Phase 2 adds the async
//! enrichment workers (embeddings, semantic tags) behind the same channel.
//!
//! Deliberately std-only (no tokio yet): one accept loop, short-lived
//! connections, a bounded channel to the writer. The dependency cost of an
//! async runtime buys nothing at this fan-in. Phase 3 adds a small axum
//! server (`ui` module) for the visual context manager, but it runs on its
//! own thread with its own runtime — it never touches this accept loop.
//!
//! This binary is a thin wrapper: `enrich`, `ui`, `writer`, `security`,
//! and the socket line protocol (`socket`) all live in `src/lib.rs`
//! (crate `ngd`) so tests can exercise them directly. Only the socket
//! accept loop and the writer thread's glue stay here, since nothing
//! external needs to drive those directly.

use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{sync_channel, Receiver};

use anyhow::Context;
use ng_core::{paths, Store};
use ngd::socket::{handle_conn, Msg};
use ngd::{enrich, ui, writer};

fn main() -> anyhow::Result<()> {
    let data_dir = paths::data_dir();
    std::fs::create_dir_all(&data_dir)?;

    // Exclusive startup lock: hooks auto-spawn ngd, so N parallel sessions
    // may race N daemons here. flock is atomic; losers exit quietly.
    let lock_file = std::fs::File::create(data_dir.join("ngd.lock"))?;
    let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        return Ok(()); // another ngd is starting or running
    }

    let socket = paths::socket_path();
    // A stale socket file from a crashed daemon blocks bind; if nothing is
    // listening on it, remove and rebind. Safe under the flock above.
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            anyhow::bail!("ngd already running on {}", socket.display());
        }
        std::fs::remove_file(&socket)?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    std::fs::write(paths::pid_path(), std::process::id().to_string())?;

    let (tx, rx) = sync_channel::<Msg>(1024);
    let writer = std::thread::spawn(move || writer_loop(rx));

    let enrich_db_path = paths::db_path();
    std::thread::spawn(move || enrich::run(enrich_db_path));

    // Own thread + own tokio runtime: the daemon's socket accept loop below
    // stays std-only, so an async runtime panic/stall in the UI server can
    // never take down event capture.
    std::thread::spawn(ui::run);

    eprintln!("ngd listening on {}", socket.display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let tx = tx.clone();
                std::thread::spawn(move || handle_conn(stream, tx));
            }
            Err(err) => eprintln!("ngd: accept error: {err}"),
        }
    }
    drop(tx);
    let _ = writer.join();
    Ok(())
}

/// Single writer: serializes all inserts so SQLite never sees write
/// contention from the daemon side. A hook that reached this point already
/// believes its event was captured, so an insert that still fails after
/// [`writer::insert_with_retry`]'s retries is never just logged and
/// dropped — it's appended to a dead-letter file instead (see
/// `writer::append_dead_letter`), recoverable later instead of lost.
fn writer_loop(rx: Receiver<Msg>) {
    let store = match Store::open(&paths::db_path()) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("ngd: cannot open db: {err}");
            return;
        }
    };
    let dead_letter_path = paths::data_dir().join("dead-letter.jsonl");
    while let Ok(msg) = rx.recv() {
        let event = match msg {
            Msg::Event(event) => event,
            Msg::Gain(record) => {
                // Metric, not memory: one cheap INSERT, no retries, no
                // dead-letter — a lost gain row is acceptable by design
                // (plano 003 A.2), a lost event is not.
                if let Err(err) = store.insert_gain(&record) {
                    eprintln!("ngd: gain_ledger insert failed (metric dropped): {err}");
                }
                continue;
            }
        };
        if let Err(err) = writer::insert_with_retry(&store, &event) {
            eprintln!(
                "ngd: insert failed after {} attempts ({err}); writing to dead-letter",
                writer::INSERT_RETRY_ATTEMPTS
            );
            if let Err(dl_err) = writer::append_dead_letter(&dead_letter_path, &event) {
                eprintln!("ngd: CRITICAL: dead-letter write also failed, event lost: {dl_err}");
            }
        }
    }
}
