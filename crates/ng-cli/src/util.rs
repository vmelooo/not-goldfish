//! Shared helpers for locating not-goldfish's sibling binaries.
//!
//! `ng`, `ng-hook`, and `ngd` are always built into the same target dir, so
//! every caller that needs one of the other two binaries (install, daemon,
//! ui, doctor) should look next to the current executable first and only
//! fall back to `PATH` — this one place is the single source of truth for
//! that lookup instead of each caller reimplementing it.

use std::path::PathBuf;

/// Look next to the current executable first, then fall back to `PATH`.
pub fn find_sibling_binary(name: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.exists() {
                return Some(sibling);
            }
        }
    }
    which(name)
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| PathBuf::from(dir).join(name))
        .find(|candidate| candidate.exists())
}
