//! DNS-rebinding and CSRF-style defenses for the local UI server.
//!
//! Binding `127.0.0.1` alone does not stop DNS rebinding: an attacker
//! registers a domain whose DNS answer flips (after the browser's
//! cache/pin expires) from a real IP to `127.0.0.1`, so a page the victim
//! already has open starts talking to this daemon while the browser still
//! believes it's on the attacker's origin. Two independent defenses close
//! that gap:
//!
//! 1. Host header allowlist ([`host_is_allowed`]): browsers always send the
//!    *original* hostname in `Host`, never the resolved IP, so a rebound
//!    request still carries `Host: evil.example:PORT` — rejecting anything
//!    that isn't exactly `127.0.0.1`/`localhost`/`[::1]` on our port defeats
//!    rebinding regardless of what DNS resolved to.
//! 2. A boot-time random token ([`generate_token`]), required as
//!    `X-NG-Token` on every state-changing request. This is defense in
//!    depth for anything that doesn't hinge on the `Host` header (a
//!    misconfigured reverse proxy, a browser that doesn't set `Host` the
//!    way we expect, ...) — not a cryptographic capability token, and not
//!    meant to resist a local attacker who can already read this process's
//!    memory or environment.

use std::io::Read;

/// True if `host_header` (the raw `Host` header value) is one of the
/// loopback forms this server accepts on `port`.
pub fn host_is_allowed(host_header: &str, port: u16) -> bool {
    let host = host_header.trim();
    let candidates = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    candidates.iter().any(|c| c.eq_ignore_ascii_case(host))
}

/// Boot-time session token: 16 bytes from the OS CSPRNG via `/dev/urandom`
/// (std-only — no `rand` dependency for one random string), hex-encoded.
/// If `/dev/urandom` is ever unreadable (locked-down container, non-Unix),
/// falls back to a process-id + high-resolution-time mix. That fallback is
/// weaker (guessable within a narrow window by another local process on
/// the same machine) but the actual threat model here is a *remote* web
/// page: DNS rebinding is defeated by the `Host` check above regardless of
/// token strength, so the token only needs to resist a blind cross-origin
/// request, not a local attacker who can already read process state.
pub fn generate_token() -> String {
    read_urandom(16)
        .map(|b| hex_encode(&b))
        .unwrap_or_else(|_| fallback_token())
}

fn read_urandom(n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

fn fallback_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    hex_encode(&(nanos ^ (pid << 64)).to_be_bytes())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compares `provided` against `expected` without an early return on the
/// first mismatched byte, so a wrong guess doesn't leak *where* it
/// diverged via response timing. Not a hardened crypto primitive — see
/// [`generate_token`]'s doc for why that tradeoff is acceptable here.
pub fn token_matches(expected: &str, provided: Option<&str>) -> bool {
    let Some(provided) = provided else {
        return false;
    };
    if provided.len() != expected.len() {
        return false;
    }
    expected
        .bytes()
        .zip(provided.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_allows_loopback_forms() {
        assert!(host_is_allowed("127.0.0.1:4949", 4949));
        assert!(host_is_allowed("localhost:4949", 4949));
        assert!(host_is_allowed("[::1]:4949", 4949));
        assert!(
            host_is_allowed("LOCALHOST:4949", 4949),
            "Host header comparison must be case-insensitive"
        );
    }

    #[test]
    fn host_rejects_foreign_host_or_wrong_port() {
        assert!(!host_is_allowed("evil.example:4949", 4949));
        assert!(!host_is_allowed("127.0.0.1:9999", 4949));
        assert!(!host_is_allowed("attacker.com", 4949));
        assert!(!host_is_allowed("", 4949));
        assert!(
            !host_is_allowed("127.0.0.1.evil.com:4949", 4949),
            "must not substring-match a lookalike host"
        );
    }

    #[test]
    fn generated_tokens_are_nonempty_and_differ_across_calls() {
        let a = generate_token();
        let b = generate_token();
        assert!(!a.is_empty());
        assert_eq!(a.len(), 32, "16 bytes hex-encoded is 32 chars");
        assert_ne!(a, b, "two boot tokens should not collide");
    }

    #[test]
    fn token_matches_exact_value_only() {
        let t = "abc123";
        assert!(token_matches(t, Some("abc123")));
        assert!(!token_matches(t, Some("abc124")));
        assert!(!token_matches(t, Some("abc12")));
        assert!(!token_matches(t, Some("abc1234")));
        assert!(!token_matches(t, None));
        assert!(!token_matches(t, Some("")));
    }
}
