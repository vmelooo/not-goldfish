//! Library surface for ng-hook.
//!
//! `main.rs` stays the actual hot-path binary; this crate exists so hook
//! handlers with real logic (as opposed to the ultra-thin capture path)
//! can be exercised by integration tests without spawning the binary and
//! wiring up a stdin pipe.

pub mod inject;
pub mod precompact;
