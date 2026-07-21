//! ngd's library surface.
//!
//! `src/main.rs` is a thin binary wrapper around this crate: the socket
//! accept loop (std-only, Phase 1) stays in the binary since nothing needs
//! to test it directly, but every module that a test needs to exercise in
//! isolation — the UI's axum router (`ui`), the writer's retry/dead-letter
//! logic (`writer`), the DNS-rebinding/token guards (`security`), and the
//! background embedding worker (`enrich`) — lives here so
//! `crates/ngd/tests/*.rs` integration tests can depend on `ngd` as an
//! ordinary library crate instead of only unit-testing pure functions.

pub mod assist;
pub mod enrich;
pub mod security;
pub mod socket;
pub(crate) mod stub;
pub mod ui;
pub mod writer;
