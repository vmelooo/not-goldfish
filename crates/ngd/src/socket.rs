//! The daemon socket's line protocol: parse newline-delimited JSON from a
//! connection and hand each message to the single-writer channel. Lives in
//! the lib (not `main.rs`) so tests can drive [`handle_conn`]'s logic
//! against an in-memory reader instead of a real daemon.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::SyncSender;

use ng_core::{Event, GainEnvelope, GainRecord};

/// Cap of bytes per *line* read from the socket. A bare
/// `BufReader::lines()` buffers the whole line with no limit, so any local
/// process could connect and stream gigabytes without a `\n`, growing the
/// daemon's memory unboundedly. 16 MiB is far above the largest legitimate
/// line (event content is already capped at
/// `ng_core::event::MAX_CONTENT_BYTES` = 256 KiB, plus JSON envelope and
/// escaping) while keeping per-connection memory bounded.
pub const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;

/// One line received on the socket: a captured event (the common case) or a
/// gain-ledger record (metric, sent by `ng-hook` after serving an injection
/// or applying hygiene). Kept as one channel so the single-writer invariant
/// holds for both tables.
pub enum Msg {
    Event(Event),
    Gain(GainRecord),
}

/// Parse newline-delimited JSON from one connection until EOF or error,
/// forwarding each message to `tx`.
pub fn handle_conn(stream: UnixStream, tx: SyncSender<Msg>) {
    handle_lines(BufReader::new(stream), tx, MAX_LINE_BYTES);
}

/// The actual loop, generic over the reader (and with the line cap as a
/// parameter) so tests don't need a socket or a 16 MiB payload.
fn handle_lines(mut reader: impl BufRead, tx: SyncSender<Msg>, max_line_bytes: u64) {
    let mut line = String::new();
    loop {
        line.clear();
        // `.take()` re-created per line: the cap bounds each *line*, not
        // the whole connection, so a client sending many events on one
        // connection is never cut short.
        let n = match (&mut reader).take(max_line_bytes).read_line(&mut line) {
            Ok(0) => return, // clean EOF
            Ok(n) => n,
            Err(_) => return, // I/O error or invalid UTF-8: drop connection
        };
        if n as u64 == max_line_bytes && !line.ends_with('\n') {
            // The line hit the cap without a terminator: there is no way to
            // resynchronize mid-line, so the whole connection is dropped.
            return;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A gain line is a `{"ng_gain": {...}}` envelope; an event has none
        // of that shape, so the two parses can never claim each other's
        // lines (see `GainEnvelope`'s doc comment).
        if let Ok(envelope) = serde_json::from_str::<GainEnvelope>(line) {
            if tx.send(Msg::Gain(envelope.ng_gain)).is_err() {
                return;
            }
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(event) => {
                if tx.send(Msg::Event(event.cap_content())).is_err() {
                    return;
                }
            }
            Err(err) => eprintln!("ngd: bad event json: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc::sync_channel;

    fn event_line(content: &str) -> String {
        serde_json::json!({
            "session_id": "s1",
            "project": "/tmp/p",
            "harness": "claude-code",
            "kind": "prompt",
            "content": content,
            "created_at": 1,
        })
        .to_string()
    }

    #[test]
    fn oversized_line_drops_the_connection_without_delivering() {
        let (tx, rx) = sync_channel(16);
        // 1000 bytes without structure against a 64-byte cap: the reader
        // must stop at the cap (bounded memory) and drop the connection.
        let huge = format!("{}\n{}\n", "x".repeat(1000), event_line("after"));
        handle_lines(Cursor::new(huge), tx, 64);
        assert!(
            rx.try_recv().is_err(),
            "nothing after an over-cap line is trustworthy"
        );
    }

    #[test]
    fn oversized_line_without_any_newline_still_returns() {
        let (tx, rx) = sync_channel(16);
        handle_lines(Cursor::new("y".repeat(1000)), tx, 64);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cap_is_per_line_not_per_connection() {
        let (tx, rx) = sync_channel(16);
        let a = event_line("one");
        let b = event_line("two");
        assert!(a.len() < 200, "each line individually under the cap");
        // Two lines whose *sum* exceeds the cap: a total-bytes cap would
        // cut the second one off; a per-line cap must deliver both.
        let both = format!("{a}\n{b}\n");
        let cap = (a.len() + 20) as u64;
        assert!((both.len() as u64) > cap);
        handle_lines(Cursor::new(both), tx, cap);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2, "both events must arrive");
    }

    #[test]
    fn events_and_gain_lines_parse_and_empty_lines_are_skipped() {
        let (tx, rx) = sync_channel(16);
        let gain = r#"{"ng_gain":{"kind":"inject","session_id":"s1","project":"","tokens":10,"items":1,"created_at":1}}"#;
        let input = format!("\n{}\n{gain}\n", event_line("hello"));
        handle_lines(Cursor::new(input), tx, MAX_LINE_BYTES);
        let msgs: Vec<_> = rx.try_iter().collect();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0], Msg::Event(e) if e.content == "hello"));
        assert!(matches!(&msgs[1], Msg::Gain(_)));
    }
}
